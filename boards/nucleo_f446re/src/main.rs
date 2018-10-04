//! Board file for Nucleo-F446RE development board
//!
//! - <https://www.st.com/en/evaluation-tools/nucleo-f446re.html>

#![no_std]
#![no_main]
#![feature(core_intrinsics, panic_implementation)]
#![deny(missing_docs)]

extern crate cortexm4;
#[macro_use(create_capability, static_init)]
extern crate kernel;
extern crate stm32f446re;

use kernel::capabilities;
use kernel::Platform;

/// Support routines for debugging I/O.
///
/// Note: USART configuration not yet implemented.
pub mod io;

// State for loading and holding applications.

// Number of concurrent processes this platform supports.
const NUM_PROCS: usize = 0;

// Actual memory for holding the active process structures.
static mut PROCESSES: [Option<&'static kernel::procs::ProcessType>; NUM_PROCS] = [];

/// Dummy buffer that causes the linker to reserve enough space for the stack.
#[no_mangle]
#[link_section = ".stack_buffer"]
pub static mut STACK_MEMORY: [u8; 0x1000] = [0; 0x1000];

/// A structure representing this platform that holds references to all
/// capsules for this platform
struct NucleoF446RE {
    ipc: kernel::ipc::IPC,
}

/// Mapping of integer syscalls to objects that implement syscalls.
impl Platform for NucleoF446RE {
    fn with_driver<F, R>(&self, driver_num: usize, f: F) -> R
    where
        F: FnOnce(Option<&kernel::Driver>) -> R,
    {
        match driver_num {
            kernel::ipc::DRIVER_NUM => f(Some(&self.ipc)),
            _ => f(None),
        }
    }
}

/// Helper function called during bring-up that configures multiplexed I/O.
unsafe fn set_pin_primary_functions() {
    //
    // No pins are being set right now!
    //
}

/// Reset Handler.
///
/// This symbol is loaded into vector table by the STM32F446RE chip crate.
/// When the chip first powers on or later does a hard reset, after the core
/// initializes all the hardware, the address of this function is loaded and
/// execution begins here.
#[no_mangle]
pub unsafe fn reset_handler() {
    stm32f446re::init();

    set_pin_primary_functions();

    let board_kernel = static_init!(kernel::Kernel, kernel::Kernel::new(&PROCESSES));

    // Create capabilities that the board need to call certain protected kernel
    // functions.
    let main_loop_capability = create_capability!(capabilities::MainLoopCapability);
    let memory_allocation_capability = create_capability!(capabilities::MemoryAllocationCapability);

    let mut chip = stm32f446re::chip::Stm32f446re::new();

    let nucleo_f446re = NucleoF446RE {
        ipc: kernel::ipc::IPC::new(board_kernel, &memory_allocation_capability),
    };

    board_kernel.kernel_loop(
        &nucleo_f446re,
        &mut chip,
        Some(&nucleo_f446re.ipc),
        &main_loop_capability,
    );
}
