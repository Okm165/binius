// Copyright 2024-2025 Irreducible Inc.

use binius_field::{
	as_packed_field::{PackScalar, PackedType},
	underlier::UnderlierType,
	ExtensionField, PackedFieldIndexable, TowerField,
};
use binius_hal::ComputationBackend;
use binius_math::EvaluationDomainFactory;
use tracing::instrument;

use super::error::Error;
use crate::{
	fiat_shamir::CanSample,
	oracle::MultilinearOracleSet,
	protocols::evalcheck::{
		serialize_evalcheck_proof, subclaims::prove_bivariate_sumchecks_with_switchover,
		EvalcheckMultilinearClaim, EvalcheckProver,
	},
	transcript::{write_u64, AdviceWriter, CanWrite},
	witness::MultilinearExtensionIndex,
};

#[allow(clippy::too_many_arguments)]
#[instrument(skip_all, name = "greedy_evalcheck::prove")]
pub fn prove<U, F, DomainField, Transcript, Backend>(
	oracles: &mut MultilinearOracleSet<F>,
	witness_index: &mut MultilinearExtensionIndex<U, F>,
	claims: impl IntoIterator<Item = EvalcheckMultilinearClaim<F>>,
	switchover_fn: impl Fn(usize) -> usize + Clone + 'static,
	transcript: &mut Transcript,
	advice: &mut AdviceWriter,
	domain_factory: impl EvaluationDomainFactory<DomainField>,
	backend: &Backend,
) -> Result<Vec<EvalcheckMultilinearClaim<F>>, Error>
where
	U: UnderlierType + PackScalar<F> + PackScalar<DomainField>,
	F: TowerField + ExtensionField<DomainField>,
	PackedType<U, F>: PackedFieldIndexable,
	DomainField: TowerField,
	Transcript: CanSample<F> + CanWrite,
	Backend: ComputationBackend,
{
	let mut evalcheck_prover =
		EvalcheckProver::<U, F, Backend>::new(oracles, witness_index, backend);

	let claims: Vec<_> = claims.into_iter().collect();

	// Prove the initial evalcheck claims
	let evalcheck_proofs = evalcheck_prover.prove(claims)?;
	write_u64(advice, evalcheck_proofs.len() as u64);
	for evalcheck_proof in evalcheck_proofs.iter() {
		serialize_evalcheck_proof(transcript, evalcheck_proof)
	}

	let mut virtual_opening_proofs_len = 0;
	loop {
		let new_sumchecks = evalcheck_prover.take_new_sumchecks_constraints().unwrap();
		if new_sumchecks.is_empty() {
			break;
		}

		// Reduce the new sumcheck claims for virtual polynomial openings to new evalcheck claims.
		let new_evalcheck_claims =
			prove_bivariate_sumchecks_with_switchover::<_, _, DomainField, _, _>(
				evalcheck_prover.oracles,
				evalcheck_prover.witness_index,
				new_sumchecks,
				transcript,
				switchover_fn.clone(),
				domain_factory.clone(),
				backend,
			)?;

		let new_evalcheck_proofs = evalcheck_prover.prove(new_evalcheck_claims)?;

		for evalcheck_proof in new_evalcheck_proofs.iter() {
			serialize_evalcheck_proof(transcript, evalcheck_proof);
		}
		virtual_opening_proofs_len += 1;
	}
	write_u64(advice, virtual_opening_proofs_len);

	let committed_claims = evalcheck_prover
		.committed_eval_claims_mut()
		.drain(..)
		.collect::<Vec<_>>();
	Ok(committed_claims)
}
