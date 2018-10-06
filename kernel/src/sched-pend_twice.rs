//! Tock core scheduler.

// Temporarily get rid of warnings
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_variables)]

use core::cell::Cell;
use core::ptr;
use core::ptr::NonNull;

use cortexmnvic;

use callback;
use callback::{AppId, Callback};
use capabilities;
use common::cells::NumericCellExt;
use grant::Grant;
use ipc;
use mem::AppSlice;
use memop;
use platform::mpu::MPU;
use platform::systick::SysTick;
use platform::{Chip, Platform};
use process::{self, Task};
use returncode::ReturnCode;
use syscall::{ContextSwitchReason, Syscall};

/// The time a process is permitted to run before being pre-empted
const KERNEL_TICK_DURATION_US: u32 = 10000;
/// Skip re-scheduling a process if its quanta is nearly exhausted
const MIN_QUANTA_THRESHOLD_US: u32 = 500;

/// Main object for the kernel. Each board will need to create one.
pub struct Kernel {
    /// How many "to-do" items exist at any given time. These include
    /// outstanding callbacks and processes in the Running state.
    work: Cell<usize>,
    /// This holds a pointer to the static array of Process pointers.
    processes: &'static [Option<&'static process::ProcessType>],
    /// How many grant regions have been setup. This is incremented on every
    /// call to `create_grant()`. We need to explicitly track this so that when
    /// processes are created they can allocated pointers for each grant.
    grant_counter: Cell<usize>,
    /// Flag to mark that grants have been finalized. This means that the kernel
    /// cannot support creating new grants because processes have already been
    /// created and the data structures for grants have already been
    /// established.
    grants_finalized: Cell<bool>,
}

impl Kernel {
    pub fn new(processes: &'static [Option<&'static process::ProcessType>]) -> Kernel {
        Kernel {
            work: Cell::new(0),
            processes: processes,
            grant_counter: Cell::new(0),
            grants_finalized: Cell::new(false),
        }
    }

    /// Something was scheduled for a process, so there is more work to do.
    crate fn increment_work(&self) {
        self.work.increment();
    }

    /// Something finished for a process, so we decrement how much work there is
    /// to do.
    crate fn decrement_work(&self) {
        self.work.decrement();
    }

    /// Helper function for determining if we should service processes or go to
    /// sleep.
    fn processes_blocked(&self) -> bool {
        self.work.get() == 0
    }

    /// Run a closure on a specific process if it exists. If the process does
    /// not exist (i.e. it is `None` in the `processes` array) then `default`
    /// will be returned. Otherwise the closure will executed and passed a
    /// reference to the process.
    crate fn process_map_or<F, R>(&self, default: R, process_index: usize, closure: F) -> R
    where
        F: FnOnce(&process::ProcessType) -> R,
    {
        if process_index > self.processes.len() {
            return default;
        }
        self.processes[process_index].map_or(default, |process| closure(process))
    }

    /// Run a closure on every valid process. This will iterate the array of
    /// processes and call the closure on every process that exists.
    crate fn process_each_enumerate<F>(&self, closure: F)
    where
        F: Fn(usize, &process::ProcessType),
    {
        for (i, process) in self.processes.iter().enumerate() {
            match process {
                Some(p) => {
                    closure(i, *p);
                }
                None => {}
            }
        }
    }

    /// Run a closure on every process, but only continue if the closure returns
    /// `FAIL`. That is, if the closure returns any other return code than
    /// `FAIL`, that value will be returned from this function and the iteration
    /// of the array of processes will stop.
    crate fn process_each_enumerate_stop<F>(&self, closure: F) -> ReturnCode
    where
        F: Fn(usize, &process::ProcessType) -> ReturnCode,
    {
        for (i, process) in self.processes.iter().enumerate() {
            match process {
                Some(p) => {
                    let ret = closure(i, *p);
                    if ret != ReturnCode::FAIL {
                        return ret;
                    }
                }
                None => {}
            }
        }
        ReturnCode::FAIL
    }

    /// Return how many processes this board supports.
    crate fn number_of_process_slots(&self) -> usize {
        self.processes.len()
    }

    /// Create a new grant. This is used in board initialization to setup grants
    /// that capsules use to interact with processes.
    ///
    /// Grants **must** only be created _before_ processes are initialized.
    /// Processes use the number of grants that have been allocated to correctly
    /// initialize the process's memory with a pointer for each grant. If a
    /// grant is created after processes are initialized this will panic.
    ///
    /// Calling this function is restricted to only certain users, and to
    /// enforce this calling this function requires the
    /// `MemoryAllocationCapability` capability.
    pub fn create_grant<T: Default>(
        &'static self,
        _capability: &capabilities::MemoryAllocationCapability,
    ) -> Grant<T> {
        if self.grants_finalized.get() {
            panic!("Grants finalized. Cannot create a new grant.");
        }

        // Create and return a new grant.
        let grant_index = self.grant_counter.get();
        self.grant_counter.increment();
        Grant::new(self, grant_index)
    }

    /// Returns the number of grants that have been setup in the system and
    /// marks the grants as "finalized". This means that no more grants can
    /// be created because data structures have been setup based on the number
    /// of grants when this function is called.
    ///
    /// In practice, this is called when processes are created, and the process
    /// memory is setup based on the number of current grants.
    crate fn get_grant_count_and_finalize(&self) -> usize {
        self.grants_finalized.set(true);
        self.grant_counter.get()
    }

    /// Cause all apps to fault.
    ///
    /// This will call `set_fault_state()` on each app, causing the app to enter
    /// the state as if it had crashed (for example with an MPU violation). If
    /// the process is configured to be restarted it will be.
    ///
    /// Only callers with the `ProcessManagementCapability` can call this
    /// function. This restricts general capsules from being able to call this
    /// function, since capsules should not be able to arbitrarily restart all
    /// apps.
    pub fn hardfault_all_apps<C: capabilities::ProcessManagementCapability>(&self, _c: &C) {
        for p in self.processes.iter() {
            p.map(|process| {
                process.set_fault_state();
            });
        }
    }

    /// Main loop.
    pub fn kernel_loop<P: Platform, C: Chip>(
        &'static self,
        platform: &P,
        chip: &C,
        ipc: Option<&ipc::IPC>,
        _capability: &capabilities::MainLoopCapability,
    ) {
        unsafe {
            static mut IRQ_10_PEND_ONCE: bool = false;

            static mut IRQ_10_PEND_TWICE: bool = false;

            // Test pending behavior - For example for IRQ 10
            let irq_10 = cortexmnvic::nvic::Nvic::new(10);

            // Helps IRQ_10_PEND_{ONCE, TWICE} values from GDB
            ptr::write_volatile(&mut IRQ_10_PEND_ONCE as *mut bool, false);

            ptr::write_volatile(&mut IRQ_10_PEND_TWICE as *mut bool, false);

            // Main kernel loop
            loop {
                // Simulate Sam4l's `service_pending_interrupts` behavior, which
                // is to -
                //
                // 1. check if there is a pending interrupt
                //
                // 2. handle the interrupt
                //
                // 3. clear pending
                //
                // 4. enable the interrupt (which was disabled by generic_isr)
                //
                {
                    loop {
                        if let Some(interrupt) = cortexmnvic::nvic::next_pending() {
                            match interrupt {
                                10 => {
                                    asm!("nop" :::: "volatile");
                                }
                                _ => {
                                    panic!("unhandled interrupt {}", interrupt);
                                }
                            }
                            let n = cortexmnvic::nvic::Nvic::new(interrupt);
                            n.clear_pending();
                            n.enable();
                        } else {
                            break;
                        }
                    }
                }

                // PRIMASK is enabled and disabled within `chip.atomic` closure
                // Pend IRQ_10 twice
                chip.atomic(|| {
                    // First time
                    if !ptr::read_volatile(&IRQ_10_PEND_ONCE as *const bool)
                        && !ptr::read_volatile(&IRQ_10_PEND_TWICE as *const bool)
                    {
                        // Set IRQ 10 as pending. As soon as PRIMASK is
                        // re-enabled, upon exiting the closure, `generic_isr`
                        // should be called provided IRQ 10 is enabled
                        // NVIC.ICER, which it would be for the first time.
                        irq_10.set_pending();

                        ptr::write_volatile(&mut IRQ_10_PEND_ONCE as *mut bool, true);
                    }

                    // Second time (Ensure that `generic_isr` has cleared the
                    // pending status)
                    if !irq_10.get_pending() {
                        if ptr::read_volatile(&IRQ_10_PEND_ONCE as *const bool)
                            && !ptr::read_volatile(&IRQ_10_PEND_TWICE as *const bool)
                        {
                            // Set IRQ 10 as pending. As soon as PRIMASK is
                            // disabled, upon exiting the closure, `generic_isr`
                            // will *not* be called as NVIC.ICER has been
                            // disabled by `generic_isr`.
                            irq_10.set_pending();

                            ptr::write_volatile(&mut IRQ_10_PEND_TWICE as *mut bool, true);
                        }
                    }
                });

                asm!("nop" :::: "volatile");

                let tmp = irq_10.get_pending();

                asm!("nop" :::: "volatile");
            }
        }
    }

    unsafe fn do_process<P: Platform, C: Chip>(
        &self,
        platform: &P,
        chip: &C,
        process: &process::ProcessType,
        appid: AppId,
        ipc: Option<&::ipc::IPC>,
    ) {
        let systick = chip.systick();
        systick.reset();
        systick.set_timer(KERNEL_TICK_DURATION_US);
        systick.enable(true);

        loop {
            if chip.has_pending_interrupts()
                || systick.overflowed()
                || !systick.greater_than(MIN_QUANTA_THRESHOLD_US)
            {
                break;
            }

            match process.get_state() {
                process::State::Running => {
                    // Running means that this process expects to be running,
                    // so go ahead and set things up and switch to executing
                    // the process.
                    process.setup_mpu(chip.mpu());
                    chip.mpu().enable_mpu();
                    systick.enable(true);
                    let context_switch_reason = process.switch_to();
                    systick.enable(false);
                    chip.mpu().disable_mpu();

                    // Now the process has returned back to the kernel. Check
                    // why and handle the process as appropriate.
                    match context_switch_reason {
                        Some(ContextSwitchReason::Fault) => {
                            // Let process deal with it as appropriate.
                            process.set_fault_state();
                        }
                        Some(ContextSwitchReason::SyscallFired) => {
                            // Handle each of the syscalls.
                            match process.get_syscall() {
                                Some(Syscall::MEMOP { operand, arg0 }) => {
                                    let res = memop::memop(process, operand, arg0);
                                    process.set_syscall_return_value(res.into());
                                }
                                Some(Syscall::YIELD) => {
                                    process.set_yielded_state();
                                    process.pop_syscall_stack_frame();

                                    // There might be already enqueued callbacks
                                    continue;
                                }
                                Some(Syscall::SUBSCRIBE {
                                    driver_number,
                                    subdriver_number,
                                    callback_ptr,
                                    appdata,
                                }) => {
                                    let callback_ptr = NonNull::new(callback_ptr);
                                    let callback = callback_ptr
                                        .map(|ptr| Callback::new(appid, appdata, ptr.cast()));

                                    let res =
                                        platform.with_driver(
                                            driver_number,
                                            |driver| match driver {
                                                Some(d) => {
                                                    d.subscribe(subdriver_number, callback, appid)
                                                }
                                                None => ReturnCode::ENODEVICE,
                                            },
                                        );
                                    process.set_syscall_return_value(res.into());
                                }
                                Some(Syscall::COMMAND {
                                    driver_number,
                                    subdriver_number,
                                    arg0,
                                    arg1,
                                }) => {
                                    let res =
                                        platform.with_driver(
                                            driver_number,
                                            |driver| match driver {
                                                Some(d) => {
                                                    d.command(subdriver_number, arg0, arg1, appid)
                                                }
                                                None => ReturnCode::ENODEVICE,
                                            },
                                        );
                                    process.set_syscall_return_value(res.into());
                                }
                                Some(Syscall::ALLOW {
                                    driver_number,
                                    subdriver_number,
                                    allow_address,
                                    allow_size,
                                }) => {
                                    let res = platform.with_driver(driver_number, |driver| {
                                        match driver {
                                            Some(d) => {
                                                if allow_address != ptr::null_mut() {
                                                    if process.in_app_owned_memory(
                                                        allow_address,
                                                        allow_size,
                                                    ) {
                                                        let slice = AppSlice::new(
                                                            allow_address,
                                                            allow_size,
                                                            appid,
                                                        );
                                                        d.allow(
                                                            appid,
                                                            subdriver_number,
                                                            Some(slice),
                                                        )
                                                    } else {
                                                        ReturnCode::EINVAL /* memory not allocated to process */
                                                    }
                                                } else {
                                                    d.allow(appid, subdriver_number, None)
                                                }
                                            }
                                            None => ReturnCode::ENODEVICE,
                                        }
                                    });
                                    process.set_syscall_return_value(res.into());
                                }
                                _ => {}
                            }
                        }
                        Some(ContextSwitchReason::TimesliceExpired) => {
                            // break to handle other processes.
                            break;
                        }
                        None => {
                            // Something went wrong when switching to this
                            // process. Indicate this by putting it in a fault
                            // state.
                            process.set_fault_state();
                        }
                    }
                }
                process::State::Yielded => match process.dequeue_task() {
                    // If the process is yielded it might be waiting for a
                    // callback. If there is a task scheduled for this process
                    // go ahead and set the process to execute it.
                    None => break,
                    Some(cb) => match cb {
                        Task::FunctionCall(ccb) => {
                            process.push_function_call(ccb);
                        }
                        Task::IPC((otherapp, ipc_type)) => {
                            ipc.map_or_else(
                                || {
                                    assert!(
                                        false,
                                        "Kernel consistency error: IPC Task with no IPC"
                                    );
                                },
                                |ipc| {
                                    ipc.schedule_callback(appid, otherapp, ipc_type);
                                },
                            );
                        }
                    },
                },
                process::State::Fault => {
                    // We should never be scheduling a process in fault.
                    panic!("Attempted to schedule a faulty process");
                }
            }
        }
        systick.reset();
    }
}
