// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

#![warn(clippy::pedantic)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![forbid(clippy::as_conversions)]
#![forbid(clippy::borrow_as_ptr)]
#![forbid(clippy::tests_outside_test_module)]
#![forbid(clippy::undocumented_unsafe_blocks)]
#![warn(clippy::debug_assert_with_mut_call)]
#![warn(clippy::error_impl_error)]
#![warn(clippy::exit)]
#![warn(
    clippy::partial_pub_fields,
    reason = "private fields are usually extra state that'd need to somehow be kept in lockstep with the arbitrarily user modifiable public state"
)]
#![warn(clippy::str_to_string)]
#![warn(clippy::useless_let_if_seq)]
#![allow(clippy::must_use_candidate)]

pub mod debounce;
pub mod sim;
