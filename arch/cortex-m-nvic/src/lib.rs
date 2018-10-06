//! Cortex-M NVIC Test

#![crate_name = "cortexmnvic"]
#![crate_type = "rlib"]
#![no_std]
#![feature(const_fn)]

extern crate tock_cells;

pub mod nvic;

mod static_ref;
