// Copyright 2024-2025 Irreducible Inc.

use binius_core::oracle::OracleId;
use binius_field::{
	as_packed_field::PackScalar, underlier::UnderlierType, BinaryField1b, TowerField,
};
use binius_macros::arith_expr;
use bytemuck::Pod;
use rayon::prelude::*;

use crate::builder::ConstraintSystemBuilder;

pub fn and<U, F>(
	builder: &mut ConstraintSystemBuilder<U, F>,
	name: impl ToString,
	xin: OracleId,
	yin: OracleId,
) -> Result<OracleId, anyhow::Error>
where
	U: UnderlierType + Pod + PackScalar<F> + PackScalar<BinaryField1b>,
	F: TowerField,
{
	builder.push_namespace(name);
	let log_rows = builder.log_rows([xin, yin])?;
	let zout = builder.add_committed("zout", log_rows, BinaryField1b::TOWER_LEVEL);
	if let Some(witness) = builder.witness() {
		(
			witness.get::<BinaryField1b>(xin)?.as_slice::<u32>(),
			witness.get::<BinaryField1b>(yin)?.as_slice::<u32>(),
			witness
				.new_column::<BinaryField1b>(zout)
				.as_mut_slice::<u32>(),
		)
			.into_par_iter()
			.for_each(|(xin, yin, zout)| {
				*zout = (*xin) & (*yin);
			});
	}
	builder.assert_zero(
		"bitwise_and",
		[xin, yin, zout],
		arith_expr!([x, y, z] = x * y - z).convert_field(),
	);
	builder.pop_namespace();
	Ok(zout)
}

pub fn xor<U, F>(
	builder: &mut ConstraintSystemBuilder<U, F>,
	name: impl ToString,
	xin: OracleId,
	yin: OracleId,
) -> Result<OracleId, anyhow::Error>
where
	U: UnderlierType + Pod + PackScalar<F> + PackScalar<BinaryField1b>,
	F: TowerField,
{
	builder.push_namespace(name);
	let log_rows = builder.log_rows([xin, yin])?;
	let zout = builder.add_linear_combination("zout", log_rows, [(xin, F::ONE), (yin, F::ONE)])?;
	if let Some(witness) = builder.witness() {
		(
			witness.get::<BinaryField1b>(xin)?.as_slice::<u32>(),
			witness.get::<BinaryField1b>(yin)?.as_slice::<u32>(),
			witness
				.new_column::<BinaryField1b>(zout)
				.as_mut_slice::<u32>(),
		)
			.into_par_iter()
			.for_each(|(xin, yin, zout)| {
				*zout = (*xin) ^ (*yin);
			});
	}
	builder.pop_namespace();
	Ok(zout)
}

pub fn or<U, F>(
	builder: &mut ConstraintSystemBuilder<U, F>,
	name: impl ToString,
	xin: OracleId,
	yin: OracleId,
) -> Result<OracleId, anyhow::Error>
where
	U: UnderlierType + Pod + PackScalar<F> + PackScalar<BinaryField1b>,
	F: TowerField,
{
	builder.push_namespace(name);
	let log_rows = builder.log_rows([xin, yin])?;
	let zout = builder.add_committed("zout", log_rows, BinaryField1b::TOWER_LEVEL);
	if let Some(witness) = builder.witness() {
		(
			witness.get::<BinaryField1b>(xin)?.as_slice::<u32>(),
			witness.get::<BinaryField1b>(yin)?.as_slice::<u32>(),
			witness
				.new_column::<BinaryField1b>(zout)
				.as_mut_slice::<u32>(),
		)
			.into_par_iter()
			.for_each(|(xin, yin, zout)| {
				*zout = (*xin) | (*yin);
			});
	}
	builder.assert_zero(
		"bitwise_or",
		[xin, yin, zout],
		arith_expr!([x, y, z] = (x + y) + (x * y) - z).convert_field(),
	);
	builder.pop_namespace();
	Ok(zout)
}
