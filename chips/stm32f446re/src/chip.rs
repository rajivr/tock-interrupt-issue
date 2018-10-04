//! Interrupt mapping and DMA channel setup.

use cortexm4;
use kernel::Chip;

pub struct Stm32f446re {
    pub mpu: cortexm4::mpu::MPU,
    pub systick: cortexm4::systick::SysTick,
}

impl Stm32f446re {
    pub unsafe fn new() -> Stm32f446re {
        //
        // DMA gets initialized here. Right now we are not working with DMA.
        //
        Stm32f446re {
            mpu: cortexm4::mpu::MPU::new(),
            systick: cortexm4::systick::SysTick::new(),
        }
    }
}

impl Chip for Stm32f446re {
    type MPU = cortexm4::mpu::MPU;
    type SysTick = cortexm4::systick::SysTick;

    fn service_pending_interrupts(&self) {
        unsafe {
            loop {
                //
                // Deferred calls are handled here. Right now we don't have any.
                //
                if let Some(interrupt) = cortexm4::nvic::next_pending() {
                    match interrupt {
                        _ => {
                            panic!("unhandled interrupt {}", interrupt);
                        }
                    }
                //
                // Once we add external interrupts we need to call
                // `clear_pending()` and `enable()` here.
                //
                } else {
                    break;
                }
            }
        }
    }

    fn has_pending_interrupts(&self) -> bool {
        //
        // Also need to add a condition for deferred_calls here when we have
        // them.
        //
        unsafe { cortexm4::nvic::has_pending() }
    }

    fn mpu(&self) -> &cortexm4::mpu::MPU {
        &self.mpu
    }

    fn systick(&self) -> &cortexm4::systick::SysTick {
        &self.systick
    }

    fn sleep(&self) {
        //
        // Need to check from power controller/manager to see if "deep sleep" is
        // possible. If so, enable it here.
        //
        unsafe {
            cortexm4::support::wfi();
        }
    }

    unsafe fn atomic<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        cortexm4::support::atomic(f)
    }
}
