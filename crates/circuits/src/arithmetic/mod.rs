// Copyright 2024-2025 Irreducible Inc.

pub mod u32;
pub mod u128;

/// Whether to allow or disallow arithmetic overflow
#[derive(Debug, Clone, Copy)]
pub enum Flags {
	Checked,
	Unchecked,
}
