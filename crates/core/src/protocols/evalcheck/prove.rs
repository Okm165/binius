// Copyright 2024-2025 Irreducible Inc.

use binius_field::{
	as_packed_field::{PackScalar, PackedType},
	underlier::UnderlierType,
	PackedFieldIndexable, TowerField,
};
use binius_hal::ComputationBackend;
use binius_math::MultilinearExtension;
use getset::{Getters, MutGetters};
use itertools::izip;
use rayon::prelude::*;
use tracing::instrument;

use super::{
	error::Error,
	evalcheck::{EvalcheckMultilinearClaim, EvalcheckProof},
	subclaims::{calculate_projected_mles, MemoizedQueries, ProjectedBivariateMeta},
	EvalPoint, EvalPointOracleIdMap,
};
use crate::{
	oracle::{
		ConstraintSet, ConstraintSetBuilder, Error as OracleError, MultilinearOracleSet,
		MultilinearPolyOracle, ProjectionVariant,
	},
	protocols::evalcheck::subclaims::{
		packed_sumcheck_meta, process_packed_sumcheck, process_shifted_sumcheck,
		shifted_sumcheck_meta,
	},
	witness::MultilinearExtensionIndex,
};

/// A mutable prover state.
///
/// Can be persisted across [`EvalcheckProver::prove`] invocations. Accumulates
/// `new_sumchecks` bivariate sumcheck instances, as well as holds mutable references to
/// the trace (to which new oracles & multilinears may be added during proving)
#[derive(Getters, MutGetters)]
pub struct EvalcheckProver<'a, 'b, U, F, Backend>
where
	U: UnderlierType + PackScalar<F>,
	F: TowerField,
	Backend: ComputationBackend,
{
	pub(crate) oracles: &'a mut MultilinearOracleSet<F>,
	pub(crate) witness_index: &'a mut MultilinearExtensionIndex<'b, U, F>,

	#[getset(get = "pub", get_mut = "pub")]
	committed_eval_claims: Vec<EvalcheckMultilinearClaim<F>>,

	finalized_proofs: EvalPointOracleIdMap<(F, EvalcheckProof<F>), F>,

	claims_queue: Vec<EvalcheckMultilinearClaim<F>>,
	incomplete_proof_claims: EvalPointOracleIdMap<EvalcheckMultilinearClaim<F>, F>,
	#[allow(clippy::type_complexity)]
	claims_without_evals: Vec<(MultilinearPolyOracle<F>, EvalPoint<F>)>,
	claims_without_evals_dedup: EvalPointOracleIdMap<(), F>,
	projected_bivariate_claims: Vec<EvalcheckMultilinearClaim<F>>,

	new_sumchecks_constraints: Vec<ConstraintSetBuilder<F>>,
	memoized_queries: MemoizedQueries<PackedType<U, F>, Backend>,
	backend: &'a Backend,
}

impl<'a, 'b, U, F, Backend> EvalcheckProver<'a, 'b, U, F, Backend>
where
	U: UnderlierType + PackScalar<F>,
	PackedType<U, F>: PackedFieldIndexable,
	F: TowerField,
	Backend: ComputationBackend,
{
	/// Create a new prover state by tying together the mutable references to the oracle set and
	/// witness index (they need to be mutable because `new_sumcheck` reduction may add new oracles & multilinears)
	/// as well as committed eval claims accumulator.
	pub fn new(
		oracles: &'a mut MultilinearOracleSet<F>,
		witness_index: &'a mut MultilinearExtensionIndex<'b, U, F>,
		backend: &'a Backend,
	) -> Self {
		Self {
			oracles,
			witness_index,
			committed_eval_claims: Vec::new(),
			new_sumchecks_constraints: Vec::new(),
			finalized_proofs: EvalPointOracleIdMap::new(),
			claims_queue: Vec::new(),
			claims_without_evals: Vec::new(),
			claims_without_evals_dedup: EvalPointOracleIdMap::new(),
			projected_bivariate_claims: Vec::new(),
			memoized_queries: MemoizedQueries::new(),
			backend,
			incomplete_proof_claims: EvalPointOracleIdMap::new(),
		}
	}

	/// A helper method to move out sumcheck constraints
	pub fn take_new_sumchecks_constraints(&mut self) -> Result<Vec<ConstraintSet<F>>, OracleError> {
		self.new_sumchecks_constraints
			.iter_mut()
			.map(|builder| std::mem::take(builder).build_one(self.oracles))
			.filter(|constraint| !matches!(constraint, Err(OracleError::EmptyConstraintSet)))
			.rev()
			.collect()
	}

	/// Prove an evalcheck claim.
	///
	/// Given a prover state containing [`MultilinearOracleSet`] indexing into given
	/// [`MultilinearExtensionIndex`], we prove an [`EvalcheckMultilinearClaim`] (stating that given composite
	/// `poly` equals `eval` at `eval_point`) by recursively processing each of the multilinears.
	/// This way the evalcheck claim gets transformed into an [`EvalcheckProof`]
	/// and a new set of claims on:
	///  * Committed polynomial evaluations
	///  * New sumcheck constraints that need to be proven in subsequent rounds (those get appended to `new_sumchecks`)
	///
	/// All of the `new_sumchecks` constraints follow the same pattern:
	///  * they are always a product of two multilins (composition polynomial is `BivariateProduct`)
	///  * one multilin (the multiplier) is transparent (`shift_ind`, `eq_ind`, or tower basis)
	///  * other multilin is a projection of one of the evalcheck claim multilins to its first variables
	#[instrument(skip_all, name = "EvalcheckProver::prove", level = "debug")]
	pub fn prove(
		&mut self,
		evalcheck_claims: Vec<EvalcheckMultilinearClaim<F>>,
	) -> Result<Vec<EvalcheckProof<F>>, Error> {
		for claim in &evalcheck_claims {
			let id = claim.poly.id();
			self.claims_without_evals_dedup
				.insert(id, claim.eval_point.clone(), ());
		}

		// Step 1: Collect proofs
		self.claims_queue.extend(evalcheck_claims.clone());

		// Use modified BFS approach with memoization to collect proofs.
		// The `prove_multilinear` function saves a proof if it can be generated immediately; otherwise, the claim is added to `incomplete_proof_claims` and resolved after BFS.
		// Claims requiring additional evaluation are stored in `claims_without_evals` and processed in parallel.
		while !self.claims_without_evals.is_empty() || !self.claims_queue.is_empty() {
			// Prove all available claims
			while !self.claims_queue.is_empty() {
				std::mem::take(&mut self.claims_queue)
					.into_iter()
					.for_each(|claim| self.prove_multilinear(claim));
			}

			let mut deduplicated_claims_without_evals = Vec::new();

			for (poly, eval_point) in std::mem::take(&mut self.claims_without_evals) {
				if self.finalized_proofs.get(poly.id(), &eval_point).is_some() {
					continue;
				}
				if self
					.claims_without_evals_dedup
					.get(poly.id(), &eval_point)
					.is_some()
				{
					continue;
				}

				self.claims_without_evals_dedup
					.insert(poly.id(), eval_point.clone(), ());

				deduplicated_claims_without_evals.push((poly, eval_point.clone()))
			}

			let deduplicated_eval_points = deduplicated_claims_without_evals
				.iter()
				.map(|(_, eval_point)| eval_point.as_ref())
				.collect::<Vec<_>>();

			self.memoized_queries
				.memoize_query_par(deduplicated_eval_points, self.backend)?;

			// Make new evaluation claims in parallel.
			let subclaims = deduplicated_claims_without_evals
				.into_par_iter()
				.map(|(poly, eval_point)| {
					Self::make_new_eval_claim(
						poly,
						eval_point,
						self.witness_index,
						&self.memoized_queries,
					)
				})
				.collect::<Result<Vec<_>, Error>>()?;

			subclaims
				.into_iter()
				.for_each(|claim| self.prove_multilinear(claim));
		}

		let mut incomplete_proof_claims =
			std::mem::take(&mut self.incomplete_proof_claims).flatten();

		while !incomplete_proof_claims.is_empty() {
			for claim in std::mem::take(&mut incomplete_proof_claims) {
				if self.complete_proof(&claim) {
					continue;
				}
				incomplete_proof_claims.push(claim);
			}
		}

		// Step 2: Collect batch_committed_eval_claims and projected_bivariate_claims in right order

		// Since we use BFS for collecting proofs and DFS for verifying them,
		// it imposes restrictions on the correct order of collecting `batch_committed_eval_claims` and `projected_bivariate_claims`.
		// Therefore, we run a DFS to handle this.
		evalcheck_claims
			.iter()
			.cloned()
			.for_each(|claim| self.collect_projected_committed(claim));

		// Step 3: Process projected_bivariate_claims

		let projected_bivariate_metas = self
			.projected_bivariate_claims
			.iter()
			.map(|claim| Self::projected_bivariate_meta(self.oracles, claim))
			.collect::<Result<Vec<_>, Error>>()?;

		let projected_mle = calculate_projected_mles(
			&projected_bivariate_metas,
			&mut self.memoized_queries,
			&self.projected_bivariate_claims,
			self.witness_index,
			self.backend,
		)?;

		for (claim, meta, projected) in izip!(
			std::mem::take(&mut self.projected_bivariate_claims),
			projected_bivariate_metas,
			projected_mle
		) {
			self.process_sumcheck(claim, meta, projected)?;
		}

		// Step 4: Find and return the proofs of the original claims.

		Ok(evalcheck_claims
			.iter()
			.map(|claim| {
				self.finalized_proofs
					.get(claim.poly.id(), &claim.eval_point)
					.map(|(_, proof)| proof.clone())
					.expect("finalized_proofs contains all the proofs")
			})
			.collect::<Vec<_>>())
	}

	#[instrument(
		skip_all,
		name = "EvalcheckProverState::prove_multilinear",
		level = "debug"
	)]
	fn prove_multilinear(&mut self, evalcheck_claim: EvalcheckMultilinearClaim<F>) {
		let multilinear = evalcheck_claim.poly.clone();

		let multilinear_id = multilinear.id();

		let eval_point = evalcheck_claim.eval_point.clone();

		let eval = evalcheck_claim.eval;

		if self
			.finalized_proofs
			.get(multilinear_id, &eval_point)
			.is_some()
		{
			return;
		}

		if self
			.incomplete_proof_claims
			.get(multilinear_id, &eval_point)
			.is_some()
		{
			return;
		}

		use MultilinearPolyOracle::*;

		match multilinear {
			Transparent { .. } => {
				self.finalized_proofs.insert(
					multilinear_id,
					eval_point,
					(eval, EvalcheckProof::Transparent),
				);
			}

			Committed { .. } => {
				self.finalized_proofs.insert(
					multilinear_id,
					eval_point,
					(eval, EvalcheckProof::Committed),
				);
			}

			Repeating { inner, .. } => {
				let n_vars = inner.n_vars();
				let inner_eval_point = eval_point.slice(0..n_vars);
				let subclaim = EvalcheckMultilinearClaim {
					poly: (*inner).clone(),
					eval_point: inner_eval_point,
					eval,
				};
				self.incomplete_proof_claims
					.insert(multilinear_id, eval_point, evalcheck_claim);
				self.claims_queue.push(subclaim);
			}

			Shifted { .. } => {
				self.finalized_proofs.insert(
					multilinear_id,
					eval_point,
					(eval, EvalcheckProof::Shifted),
				);
			}

			Packed { .. } => {
				self.finalized_proofs.insert(
					multilinear_id,
					eval_point,
					(eval, EvalcheckProof::Packed),
				);
			}

			Projected { projected, .. } => {
				let (inner, values) = (projected.inner(), projected.values());
				let new_eval_point = match projected.projection_variant() {
					ProjectionVariant::LastVars => {
						let mut new_eval_point = eval_point.to_vec();
						new_eval_point.extend(values);
						new_eval_point
					}
					ProjectionVariant::FirstVars => {
						values.iter().cloned().chain(eval_point.to_vec()).collect()
					}
				};

				let subclaim = EvalcheckMultilinearClaim {
					poly: (**inner).clone(),
					eval_point: new_eval_point.into(),
					eval,
				};
				self.incomplete_proof_claims
					.insert(multilinear_id, eval_point, evalcheck_claim);
				self.claims_queue.push(subclaim);
			}

			LinearCombination {
				linear_combination, ..
			} => {
				let n_polys = linear_combination.n_polys();

				match linear_combination
					.polys()
					.zip(linear_combination.coefficients())
					.next()
				{
					Some((suboracle, coeff)) if n_polys == 1 && !coeff.is_zero() => {
						let eval = (eval - linear_combination.offset())
							* coeff.invert().expect("not zero");
						let subclaim = EvalcheckMultilinearClaim {
							poly: suboracle.clone(),
							eval_point: eval_point.clone(),
							eval,
						};
						self.claims_queue.push(subclaim);
					}
					_ => {
						for suboracle in linear_combination.polys() {
							self.claims_without_evals
								.push(((*suboracle).clone(), eval_point.clone()));
						}
					}
				};

				self.incomplete_proof_claims
					.insert(multilinear_id, eval_point, evalcheck_claim);
			}

			ZeroPadded { inner, .. } => {
				let inner_n_vars = inner.n_vars();

				let inner_eval_point = eval_point.slice(0..inner_n_vars);
				self.claims_without_evals
					.push(((*inner).clone(), inner_eval_point));
				self.incomplete_proof_claims
					.insert(multilinear_id, eval_point, evalcheck_claim);
			}
		};
	}

	fn complete_proof(&mut self, evalcheck_claim: &EvalcheckMultilinearClaim<F>) -> bool {
		use MultilinearPolyOracle::*;

		let multilinear = &evalcheck_claim.poly;
		let eval_point = evalcheck_claim.eval_point.clone();
		let eval = evalcheck_claim.eval;

		let res = match multilinear {
			Repeating { inner, .. } => {
				let n_vars = inner.n_vars();
				let inner_eval_point = &evalcheck_claim.eval_point[..n_vars];
				self.finalized_proofs
					.get(inner.id(), inner_eval_point)
					.map(|(_, subproof)| subproof.clone())
					.map(move |subproof| {
						let proof = EvalcheckProof::Repeating(Box::new(subproof));
						self.finalized_proofs.insert(
							evalcheck_claim.poly.id(),
							eval_point,
							(eval, proof),
						);
					})
			}
			Projected { projected, .. } => {
				let (inner, values) = (projected.inner(), projected.values());
				let new_eval_point = match projected.projection_variant() {
					ProjectionVariant::LastVars => {
						let mut new_eval_point = eval_point.to_vec();
						new_eval_point.extend(values);
						new_eval_point
					}
					ProjectionVariant::FirstVars => values
						.iter()
						.cloned()
						.chain((*eval_point).to_vec())
						.collect(),
				};
				let new_poly = inner.clone();
				self.finalized_proofs
					.get(new_poly.id(), &new_eval_point)
					.map(|(_, subproof)| subproof.clone())
					.map(|subproof| {
						self.finalized_proofs.insert(
							evalcheck_claim.poly.id(),
							eval_point,
							(eval, subproof),
						);
					})
			}

			LinearCombination {
				linear_combination, ..
			} => linear_combination
				.polys()
				.map(|suboracle| {
					self.finalized_proofs
						.get(suboracle.id(), &evalcheck_claim.eval_point)
						.map(|(eval, subproof)| (*eval, subproof.clone()))
				})
				.collect::<Option<Vec<_>>>()
				.map(|subproofs| {
					self.finalized_proofs.insert(
						evalcheck_claim.poly.id(),
						eval_point,
						(eval, EvalcheckProof::LinearCombination { subproofs }),
					);
				}),

			ZeroPadded { inner, .. } => {
				let inner_n_vars = inner.n_vars();
				let inner_eval_point = &evalcheck_claim.eval_point[..inner_n_vars];
				self.finalized_proofs
					.get(inner.id(), inner_eval_point)
					.map(|(eval, subproof)| (*eval, subproof.clone()))
					.map(|(internal_eval, subproof)| {
						self.finalized_proofs.insert(
							evalcheck_claim.poly.id(),
							eval_point,
							(eval, EvalcheckProof::ZeroPadded(internal_eval, Box::new(subproof))),
						);
					})
			}
			_ => unreachable!(),
		};
		res.is_some()
	}

	fn collect_projected_committed(&mut self, evalcheck_claim: EvalcheckMultilinearClaim<F>) {
		let EvalcheckMultilinearClaim {
			poly: multilinear,
			eval_point,
			eval,
		} = evalcheck_claim.clone();

		use MultilinearPolyOracle::*;

		match multilinear {
			Committed { .. } => {
				let subclaim = EvalcheckMultilinearClaim {
					poly: multilinear,
					eval_point,
					eval,
				};

				self.committed_eval_claims.push(subclaim);
			}
			Repeating { inner, .. } => {
				let n_vars = inner.n_vars();
				let inner_eval_point = eval_point.slice(0..n_vars);
				let subclaim = EvalcheckMultilinearClaim {
					poly: (*inner).clone(),
					eval_point: inner_eval_point,
					eval,
				};

				self.collect_projected_committed(subclaim);
			}
			Projected { projected, .. } => {
				let (inner, values) = (projected.inner(), projected.values());
				let new_eval_point = match projected.projection_variant() {
					ProjectionVariant::LastVars => {
						let mut new_eval_point = eval_point.to_vec();
						new_eval_point.extend(values);
						new_eval_point
					}
					ProjectionVariant::FirstVars => {
						values.iter().cloned().chain(eval_point.to_vec()).collect()
					}
				};

				let new_poly = (**inner).clone();

				let subclaim = EvalcheckMultilinearClaim {
					poly: new_poly,
					eval_point: new_eval_point.into(),
					eval,
				};
				self.collect_projected_committed(subclaim);
			}
			Shifted { .. } => self.projected_bivariate_claims.push(evalcheck_claim),
			Packed { .. } => self.projected_bivariate_claims.push(evalcheck_claim),
			LinearCombination {
				linear_combination, ..
			} => {
				for poly in linear_combination.polys().cloned() {
					let (eval, _) = self
						.finalized_proofs
						.get(poly.id(), &eval_point)
						.expect("finalized_proofs contains all the proofs");
					let subclaim = EvalcheckMultilinearClaim {
						poly,
						eval_point: eval_point.clone(),
						eval: *eval,
					};
					self.collect_projected_committed(subclaim);
				}
			}
			ZeroPadded { inner, .. } => {
				let inner_n_vars = inner.n_vars();
				let inner_eval_point = eval_point.slice(0..inner_n_vars);

				let (eval, _) = self
					.finalized_proofs
					.get(inner.id(), &inner_eval_point)
					.expect("finalized_proofs contains all the proofs");

				let subclaim = EvalcheckMultilinearClaim {
					poly: (*inner).clone(),
					eval_point: eval_point.clone(),
					eval: *eval,
				};
				self.collect_projected_committed(subclaim);
			}
			_ => {}
		}
	}

	fn projected_bivariate_meta(
		oracles: &mut MultilinearOracleSet<F>,
		evalcheck_claim: &EvalcheckMultilinearClaim<F>,
	) -> Result<ProjectedBivariateMeta, Error> {
		use MultilinearPolyOracle::*;

		let EvalcheckMultilinearClaim {
			poly: multilinear,
			eval_point,
			..
		} = evalcheck_claim;

		match multilinear {
			Shifted { shifted, .. } => shifted_sumcheck_meta(oracles, shifted, eval_point),

			Packed { packed, .. } => packed_sumcheck_meta(oracles, packed, eval_point),
			_ => unreachable!(),
		}
	}

	fn process_sumcheck(
		&mut self,
		evalcheck_claim: EvalcheckMultilinearClaim<F>,
		meta: ProjectedBivariateMeta,
		projected: MultilinearExtension<PackedType<U, F>>,
	) -> Result<(), Error> {
		use MultilinearPolyOracle::*;

		let EvalcheckMultilinearClaim {
			poly: multilinear,
			eval_point,
			eval,
		} = evalcheck_claim;

		match multilinear {
			Shifted { shifted, .. } => process_shifted_sumcheck(
				&shifted,
				meta,
				&eval_point,
				eval,
				self.witness_index,
				&mut self.new_sumchecks_constraints,
				projected,
			)?,

			Packed { packed, .. } => process_packed_sumcheck(
				&packed,
				meta,
				&eval_point,
				eval,
				self.witness_index,
				&mut self.new_sumchecks_constraints,
				projected,
			)?,
			_ => unreachable!(),
		};
		Ok(())
	}

	fn make_new_eval_claim(
		poly: MultilinearPolyOracle<F>,
		eval_point: EvalPoint<F>,
		witness_index: &MultilinearExtensionIndex<U, F>,
		memoized_queries: &MemoizedQueries<PackedType<U, F>, Backend>,
	) -> Result<EvalcheckMultilinearClaim<F>, Error> {
		let eval_query = memoized_queries
			.full_query_readonly(&eval_point)
			.ok_or(Error::MissingQuery)?;

		let witness_poly = witness_index
			.get_multilin_poly(poly.id())
			.map_err(Error::Witness)?;

		let eval = witness_poly
			.evaluate(eval_query.to_ref())
			.map_err(Error::from)?;

		Ok(EvalcheckMultilinearClaim {
			poly,
			eval_point,
			eval,
		})
	}
}
