// Copyright 2024-2025 Irreducible Inc.

pub mod channel;
mod common;
pub mod error;
mod prove;
pub mod validate;
mod verify;

use binius_field::TowerField;
use channel::{ChannelId, Flush};
pub use prove::prove;
pub use verify::verify;

use crate::oracle::{ConstraintSet, MultilinearOracleSet, OracleId};

/// Contains the 3 things that place constraints on witness data in Binius
/// - virtual oracles
/// - polynomial constraints
/// - channel flushes
///
/// As a result, a ConstraintSystem allows us to validate all of these
/// constraints against a witness, as well as enabling generic prove/verify
#[derive(Debug, Clone)]
pub struct ConstraintSystem<F: TowerField> {
	pub oracles: MultilinearOracleSet<F>,
	pub table_constraints: Vec<ConstraintSet<F>>,
	pub non_zero_oracle_ids: Vec<OracleId>,
	pub flushes: Vec<Flush>,
	pub max_channel_id: ChannelId,
}

impl<F: TowerField> ConstraintSystem<F> {
	pub fn no_base_constraints(self) -> ConstraintSystem<F> {
		ConstraintSystem {
			oracles: self.oracles,
			table_constraints: self.table_constraints,
			non_zero_oracle_ids: self.non_zero_oracle_ids,
			flushes: self.flushes,
			max_channel_id: self.max_channel_id,
		}
	}
}

/// Constraint system proof that has been serialized into bytes
#[derive(Debug, Clone)]
pub struct Proof {
	pub transcript: Vec<u8>,
	pub advice: Vec<u8>,
}

impl Proof {
	pub fn get_proof_size(&self) -> usize {
		self.transcript.len() + self.advice.len()
	}
}
