// Copyright (C) 2019-2022 Aleo Systems Inc.
// This file is part of the snarkVM library.

// The snarkVM library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkVM library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkVM library. If not, see <https://www.gnu.org/licenses/>.

//! Here we construct a polynomial commitment that enables users to commit to a
//! single polynomial `p`, and then later provide an evaluation proof that
//! convinces verifiers that a claimed value `v` is the true evaluation of `p`
//! at a chosen point `x`. Our construction follows the template of the construction
//! proposed by Kate, Zaverucha, and Goldberg ([KZG11](http://cacr.uwaterloo.ca/techreports/2010/cacr2010-10.pdf)).
//! This construction achieves extractability in the algebraic group model (AGM).

use crate::{
    fft::{DensePolynomial, Polynomial},
    msm::{FixedBase, VariableBase},
    polycommit::PCError,
};
use snarkvm_curves::traits::{AffineCurve, PairingCurve, PairingEngine, ProjectiveCurve};
use snarkvm_fields::{Field, One, PrimeField, Zero};
use snarkvm_parameters::testnet3::PowersOfG;
use snarkvm_utilities::{cfg_iter, rand::Uniform, BitIteratorBE};

use core::{
    marker::PhantomData,
    ops::Mul,
    sync::atomic::{AtomicBool, Ordering},
};
use itertools::Itertools;
use parking_lot::RwLock;
use rand_core::RngCore;
use std::{collections::BTreeMap, sync::Arc};

#[cfg(feature = "parallel")]
use rayon::prelude::*;

mod data_structures;
pub use data_structures::*;

use super::sonic_pc::LabeledPolynomialWithBasis;

#[derive(Debug, PartialEq, Eq)]
#[allow(deprecated)]
pub enum KZG10DegreeBoundsConfig {
    #[deprecated]
    ALL,
    MARLIN,
    LIST(Vec<usize>),
    NONE,
}

#[allow(deprecated)]
impl KZG10DegreeBoundsConfig {
    pub fn get_list<F: PrimeField>(&self, max_degree: usize) -> Vec<usize> {
        match self {
            KZG10DegreeBoundsConfig::ALL => (0..max_degree).collect(),
            KZG10DegreeBoundsConfig::MARLIN => {
                // In Marlin, the degree bounds are all of the forms `domain_size - 2`.
                // Consider that we are using radix-2 FFT,
                // there are only a few possible domain sizes and therefore degree bounds.
                //
                // We do not consider mixed-radix FFT for simplicity, as the curves that we
                // are using have very high two-arity.

                let mut radix_2_possible_domain_sizes = vec![];

                let mut cur = 2usize;
                while cur - 2 <= max_degree {
                    radix_2_possible_domain_sizes.push(cur - 2);
                    cur *= 2;
                }

                radix_2_possible_domain_sizes
            }
            KZG10DegreeBoundsConfig::LIST(v) => v.clone(),
            KZG10DegreeBoundsConfig::NONE => vec![],
        }
    }
}

/// `KZG10` is an implementation of the polynomial commitment scheme of
/// [Kate, Zaverucha and Goldbgerg][kzg10]
///
/// [kzg10]: http://cacr.uwaterloo.ca/techreports/2010/cacr2010-10.pdf
#[derive(Clone, Debug)]
pub struct KZG10<E: PairingEngine>(PhantomData<E>);

impl<E: PairingEngine> KZG10<E> {
    /// Constructs public parameters when given as input the maximum degree `degree`
    /// for the polynomial commitment scheme.
    pub fn setup<R: RngCore>(
        max_degree: usize,
        supported_degree_bounds_config: &KZG10DegreeBoundsConfig,
        produce_g2_powers: bool,
        rng: &mut R,
    ) -> Result<UniversalParams<E>, PCError> {
        if max_degree < 1 {
            return Err(PCError::DegreeIsZero);
        }
        let max_lagrange_size =
            if max_degree.is_power_of_two() { max_degree } else { max_degree.next_power_of_two() >> 1 };

        if !max_lagrange_size.is_power_of_two() {
            return Err(PCError::LagrangeBasisSizeIsNotPowerOfTwo);
        }
        if max_lagrange_size > max_degree + 1 {
            return Err(PCError::LagrangeBasisSizeIsTooLarge);
        }
        let setup_time = start_timer!(|| format!("KZG10::Setup with degree {}", max_degree));
        let scalar_bits = E::Fr::size_in_bits();

        // Compute the `toxic waste`.
        let beta = E::Fr::rand(rng);
        let g = E::G1Projective::rand(rng);
        let gamma_g = E::G1Projective::rand(rng);
        let h = E::G2Projective::rand(rng);

        // Compute `beta^i G`.
        let powers_of_beta = {
            let mut powers_of_beta = vec![E::Fr::one()];
            let mut cur = beta;
            for _ in 0..max_degree {
                powers_of_beta.push(cur);
                cur *= &beta;
            }
            powers_of_beta
        };
        let window_size = FixedBase::get_mul_window_size(max_degree + 1);
        let g_time = start_timer!(|| "Generating powers of G");
        let g_table = FixedBase::get_window_table(scalar_bits, window_size, g);
        let powers_of_beta_g = FixedBase::msm::<E::G1Projective>(scalar_bits, window_size, &g_table, &powers_of_beta);
        end_timer!(g_time);

        // Compute `gamma beta^i G`.
        let gamma_g_time = start_timer!(|| "Generating powers of gamma * G");
        let gamma_g_table = FixedBase::get_window_table(scalar_bits, window_size, gamma_g);
        let mut powers_of_beta_times_gamma_g =
            FixedBase::msm::<E::G1Projective>(scalar_bits, window_size, &gamma_g_table, &powers_of_beta);
        // Add an additional power of gamma_g, because we want to be able to support
        // up to D queries.
        powers_of_beta_times_gamma_g.push(powers_of_beta_times_gamma_g.last().unwrap().mul(beta));
        end_timer!(gamma_g_time);

        // Reduce `beta^i G` and `gamma beta^i G` to affine representations.
        let powers_of_beta_g = E::G1Projective::batch_normalization_into_affine(powers_of_beta_g);
        let powers_of_beta_times_gamma_g =
            E::G1Projective::batch_normalization_into_affine(powers_of_beta_times_gamma_g)
                .into_iter()
                .enumerate()
                .collect();

        // This part is used to derive the universal verification parameters.
        let list = supported_degree_bounds_config.get_list::<E::Fr>(max_degree);

        let supported_degree_bounds =
            if *supported_degree_bounds_config != KZG10DegreeBoundsConfig::NONE { list.clone() } else { vec![] };

        // Compute `neg_powers_of_beta_h`.
        let inverse_neg_powers_of_beta_h_time = start_timer!(|| "Generating negative powers of h in G2");
        let inverse_neg_powers_of_beta_h =
            if produce_g2_powers && *supported_degree_bounds_config != KZG10DegreeBoundsConfig::NONE {
                let mut map = BTreeMap::<usize, E::G2Affine>::new();

                let mut neg_powers_of_beta = vec![];
                for i in list.iter() {
                    neg_powers_of_beta.push(beta.pow([(max_degree - *i) as u64]).inverse().unwrap());
                }

                let window_size = FixedBase::get_mul_window_size(neg_powers_of_beta.len());
                let neg_h_table = FixedBase::get_window_table(scalar_bits, window_size, h);
                let neg_powers_of_h =
                    FixedBase::msm::<E::G2Projective>(scalar_bits, window_size, &neg_h_table, &neg_powers_of_beta);

                let affines = E::G2Projective::batch_normalization_into_affine(neg_powers_of_h);

                for (i, affine) in list.iter().zip_eq(affines.iter()) {
                    map.insert(*i, *affine);
                }

                map
            } else {
                BTreeMap::new()
            };
        end_timer!(inverse_neg_powers_of_beta_h_time);

        let beta_h = h.mul(beta).to_affine();
        let h = h.to_affine();
        let prepared_h = h.prepare();
        let prepared_beta_h = beta_h.prepare();

        let powers = PowersOfG::<E>::setup(powers_of_beta_g, powers_of_beta_times_gamma_g)?;
        let pp = UniversalParams {
            powers: Arc::new(RwLock::new(powers)),
            h,
            beta_h,
            supported_degree_bounds,
            inverse_neg_powers_of_beta_h,
            prepared_h,
            prepared_beta_h,
        };
        end_timer!(setup_time);
        Ok(pp)
    }

    /// Outputs a commitment to `polynomial`.
    pub fn commit(
        powers: &Powers<E>,
        polynomial: &Polynomial<'_, E::Fr>,
        hiding_bound: Option<usize>,
        terminator: &AtomicBool,
        rng: Option<&mut dyn RngCore>,
    ) -> Result<(Commitment<E>, Randomness<E>), PCError> {
        Self::check_degree_is_too_large(polynomial.degree(), powers.size())?;

        let commit_time = start_timer!(|| format!(
            "Committing to polynomial of degree {} with hiding_bound: {:?}",
            polynomial.degree(),
            hiding_bound,
        ));

        let mut commitment = match polynomial {
            Polynomial::Dense(polynomial) => {
                let (num_leading_zeros, plain_coeffs) = skip_leading_zeros_and_convert_to_bigints(polynomial);

                let msm_time = start_timer!(|| "MSM to compute commitment to plaintext poly");
                let commitment = VariableBase::msm(&powers.powers_of_beta_g[num_leading_zeros..], &plain_coeffs);
                end_timer!(msm_time);

                if terminator.load(Ordering::Relaxed) {
                    return Err(PCError::Terminated);
                }
                commitment
            }
            Polynomial::Sparse(polynomial) => polynomial
                .coeffs()
                .map(|(i, coeff)| {
                    powers.powers_of_beta_g[*i].mul_bits(BitIteratorBE::new_without_leading_zeros(coeff.to_repr()))
                })
                .sum(),
        };

        let mut randomness = Randomness::empty();
        if let Some(hiding_degree) = hiding_bound {
            let mut rng = rng.ok_or(PCError::MissingRng)?;
            let sample_random_poly_time =
                start_timer!(|| format!("Sampling a random polynomial of degree {}", hiding_degree));

            randomness = Randomness::rand(hiding_degree, false, &mut rng);
            Self::check_hiding_bound(
                randomness.blinding_polynomial.degree(),
                powers.powers_of_beta_times_gamma_g.len(),
            )?;
            end_timer!(sample_random_poly_time);
        }

        let random_ints = convert_to_bigints(&randomness.blinding_polynomial.coeffs);
        let msm_time = start_timer!(|| "MSM to compute commitment to random poly");
        let random_commitment =
            VariableBase::msm(&powers.powers_of_beta_times_gamma_g, random_ints.as_slice()).to_affine();
        end_timer!(msm_time);

        if terminator.load(Ordering::Relaxed) {
            return Err(PCError::Terminated);
        }

        commitment.add_assign_mixed(&random_commitment);

        end_timer!(commit_time);
        Ok((Commitment(commitment.into()), randomness))
    }

    /// Outputs a commitment to `polynomial`.
    pub fn commit_lagrange(
        lagrange_basis: &LagrangeBasis<E>,
        evaluations: &[E::Fr],
        hiding_bound: Option<usize>,
        terminator: &AtomicBool,
        rng: Option<&mut dyn RngCore>,
    ) -> Result<(Commitment<E>, Randomness<E>), PCError> {
        Self::check_degree_is_too_large(evaluations.len() - 1, lagrange_basis.size())?;
        assert_eq!(evaluations.len().next_power_of_two(), lagrange_basis.size());

        let commit_time = start_timer!(|| format!(
            "Committing to polynomial of degree {} with hiding_bound: {:?}",
            evaluations.len() - 1,
            hiding_bound,
        ));

        let evaluations = evaluations.iter().map(|e| e.to_repr()).collect::<Vec<_>>();
        let msm_time = start_timer!(|| "MSM to compute commitment to plaintext poly");
        let mut commitment = VariableBase::msm(&lagrange_basis.lagrange_basis_at_beta_g, &evaluations);
        end_timer!(msm_time);

        if terminator.load(Ordering::Relaxed) {
            return Err(PCError::Terminated);
        }

        let mut randomness = Randomness::empty();
        if let Some(hiding_degree) = hiding_bound {
            let mut rng = rng.ok_or(PCError::MissingRng)?;
            let sample_random_poly_time =
                start_timer!(|| format!("Sampling a random polynomial of degree {}", hiding_degree));

            randomness = Randomness::rand(hiding_degree, false, &mut rng);
            Self::check_hiding_bound(
                randomness.blinding_polynomial.degree(),
                lagrange_basis.powers_of_beta_times_gamma_g.len(),
            )?;
            end_timer!(sample_random_poly_time);
        }

        let random_ints = convert_to_bigints(&randomness.blinding_polynomial.coeffs);
        let msm_time = start_timer!(|| "MSM to compute commitment to random poly");
        let random_commitment =
            VariableBase::msm(&lagrange_basis.powers_of_beta_times_gamma_g, random_ints.as_slice()).to_affine();
        end_timer!(msm_time);

        if terminator.load(Ordering::Relaxed) {
            return Err(PCError::Terminated);
        }

        commitment.add_assign_mixed(&random_commitment);

        end_timer!(commit_time);
        Ok((Commitment(commitment.into()), randomness))
    }

    /// Compute witness polynomial.
    ///
    /// The witness polynomial w(x) the quotient of the division (p(x) - p(z)) / (x - z)
    /// Observe that this quotient does not change with z because
    /// p(z) is the remainder term. We can therefore omit p(z) when computing the quotient.
    #[allow(clippy::type_complexity)]
    pub fn compute_witness_polynomial(
        polynomial: &DensePolynomial<E::Fr>,
        point: E::Fr,
        randomness: &Randomness<E>,
    ) -> Result<(DensePolynomial<E::Fr>, Option<DensePolynomial<E::Fr>>), PCError> {
        let divisor = DensePolynomial::from_coefficients_vec(vec![-point, E::Fr::one()]);

        let witness_time = start_timer!(|| "Computing witness polynomial");
        let witness_polynomial = polynomial / &divisor;
        end_timer!(witness_time);

        let random_witness_polynomial = if randomness.is_hiding() {
            let random_p = &randomness.blinding_polynomial;

            let witness_time = start_timer!(|| "Computing random witness polynomial");
            let random_witness_polynomial = random_p / &divisor;
            end_timer!(witness_time);
            Some(random_witness_polynomial)
        } else {
            None
        };

        Ok((witness_polynomial, random_witness_polynomial))
    }

    pub(crate) fn open_with_witness_polynomial(
        powers: &Powers<E>,
        point: E::Fr,
        randomness: &Randomness<E>,
        witness_polynomial: &DensePolynomial<E::Fr>,
        hiding_witness_polynomial: Option<&DensePolynomial<E::Fr>>,
    ) -> Result<Proof<E>, PCError> {
        Self::check_degree_is_too_large(witness_polynomial.degree(), powers.size())?;
        let (num_leading_zeros, witness_coeffs) = skip_leading_zeros_and_convert_to_bigints(witness_polynomial);

        let witness_comm_time = start_timer!(|| "Computing commitment to witness polynomial");
        let mut w = VariableBase::msm(&powers.powers_of_beta_g[num_leading_zeros..], &witness_coeffs);
        end_timer!(witness_comm_time);

        let random_v = if let Some(hiding_witness_polynomial) = hiding_witness_polynomial {
            let blinding_p = &randomness.blinding_polynomial;
            let blinding_eval_time = start_timer!(|| "Evaluating random polynomial");
            let blinding_evaluation = blinding_p.evaluate(point);
            end_timer!(blinding_eval_time);

            let random_witness_coeffs = convert_to_bigints(&hiding_witness_polynomial.coeffs);
            let witness_comm_time = start_timer!(|| "Computing commitment to random witness polynomial");
            w += &VariableBase::msm(&powers.powers_of_beta_times_gamma_g, &random_witness_coeffs);
            end_timer!(witness_comm_time);
            Some(blinding_evaluation)
        } else {
            None
        };

        Ok(Proof { w: w.to_affine(), random_v })
    }

    /// On input a polynomial `p` and a point `point`, outputs a proof for the same.
    pub(crate) fn open(
        powers: &Powers<E>,
        polynomial: &DensePolynomial<E::Fr>,
        point: E::Fr,
        rand: &Randomness<E>,
    ) -> Result<Proof<E>, PCError> {
        Self::check_degree_is_too_large(polynomial.degree(), powers.size())?;
        let open_time = start_timer!(|| format!("Opening polynomial of degree {}", polynomial.degree()));

        let witness_time = start_timer!(|| "Computing witness polynomials");
        let (witness_poly, hiding_witness_poly) = Self::compute_witness_polynomial(polynomial, point, rand)?;
        end_timer!(witness_time);

        let proof =
            Self::open_with_witness_polynomial(powers, point, rand, &witness_poly, hiding_witness_poly.as_ref());

        end_timer!(open_time);
        proof
    }

    /// Verifies that `value` is the evaluation at `point` of the polynomial
    /// committed inside `commitment`.
    pub fn check(
        vk: &VerifierKey<E>,
        commitment: &Commitment<E>,
        point: E::Fr,
        value: E::Fr,
        proof: &Proof<E>,
    ) -> Result<bool, PCError> {
        let check_time = start_timer!(|| "Checking evaluation");
        let mut inner = commitment.0.to_projective() - vk.g.to_projective().mul(value);
        if let Some(random_v) = proof.random_v {
            inner -= &vk.gamma_g.mul(random_v);
        }
        let lhs = E::pairing(inner, vk.h);

        let inner = vk.beta_h.to_projective() - vk.h.mul(point);
        let rhs = E::pairing(proof.w, inner);

        end_timer!(check_time, || format!("Result: {}", lhs == rhs));
        Ok(lhs == rhs)
    }

    /// Check that each `proof_i` in `proofs` is a valid proof of evaluation for
    /// `commitment_i` at `point_i`.
    pub fn batch_check<R: RngCore>(
        vk: &VerifierKey<E>,
        commitments: &[Commitment<E>],
        points: &[E::Fr],
        values: &[E::Fr],
        proofs: &[Proof<E>],
        rng: &mut R,
    ) -> Result<bool, PCError> {
        let check_time = start_timer!(|| format!("Checking {} evaluation proofs", commitments.len()));
        let g = vk.g.to_projective();
        let gamma_g = vk.gamma_g.to_projective();

        let mut total_c = <E::G1Projective>::zero();
        let mut total_w = <E::G1Projective>::zero();

        let combination_time = start_timer!(|| "Combining commitments and proofs");
        let mut randomizer = E::Fr::one();
        // Instead of multiplying g and gamma_g in each turn, we simply accumulate
        // their coefficients and perform a final multiplication at the end.
        let mut g_multiplier = E::Fr::zero();
        let mut gamma_g_multiplier = E::Fr::zero();
        for (((c, z), v), proof) in commitments.iter().zip_eq(points).zip_eq(values).zip_eq(proofs) {
            let w = proof.w;
            let mut temp = w.mul(*z);
            temp.add_assign_mixed(&c.0);
            let c = temp;
            g_multiplier += &(randomizer * v);
            if let Some(random_v) = proof.random_v {
                gamma_g_multiplier += &(randomizer * random_v);
            }
            total_c += &c.mul(randomizer);
            total_w += &w.mul(randomizer);
            // We don't need to sample randomizers from the full field,
            // only from 128-bit strings.
            randomizer = u128::rand(rng).into();
        }
        total_c -= &g.mul(g_multiplier);
        total_c -= &gamma_g.mul(gamma_g_multiplier);
        end_timer!(combination_time);

        let to_affine_time = start_timer!(|| "Converting results to affine for pairing");
        let affine_points = E::G1Projective::batch_normalization_into_affine(vec![-total_w, total_c]);
        let (total_w, total_c) = (affine_points[0], affine_points[1]);
        end_timer!(to_affine_time);

        let pairing_time = start_timer!(|| "Performing product of pairings");
        let result = E::product_of_pairings(
            [(&total_w.prepare(), &vk.prepared_beta_h), (&total_c.prepare(), &vk.prepared_h)].iter().copied(),
        )
        .is_one();
        end_timer!(pairing_time);
        end_timer!(check_time, || format!("Result: {}", result));
        Ok(result)
    }

    pub(crate) fn check_degree_is_too_large(degree: usize, num_powers: usize) -> Result<(), PCError> {
        let num_coefficients = degree + 1;
        if num_coefficients > num_powers {
            Err(PCError::TooManyCoefficients { num_coefficients, num_powers })
        } else {
            Ok(())
        }
    }

    pub(crate) fn check_hiding_bound(hiding_poly_degree: usize, num_powers: usize) -> Result<(), PCError> {
        if hiding_poly_degree == 0 {
            Err(PCError::HidingBoundIsZero)
        } else if hiding_poly_degree >= num_powers {
            // The above check uses `>=` because committing to a hiding poly with
            // degree `hiding_poly_degree` requires `hiding_poly_degree + 1` powers.
            Err(PCError::HidingBoundToolarge { hiding_poly_degree, num_powers })
        } else {
            Ok(())
        }
    }

    pub(crate) fn check_degrees_and_bounds<'a>(
        supported_degree: usize,
        max_degree: usize,
        enforced_degree_bounds: Option<&[usize]>,
        p: impl Into<LabeledPolynomialWithBasis<'a, E::Fr>>,
    ) -> Result<(), PCError> {
        let p = p.into();
        if let Some(bound) = p.degree_bound() {
            let enforced_degree_bounds = enforced_degree_bounds.ok_or(PCError::UnsupportedDegreeBound(bound))?;

            if enforced_degree_bounds.binary_search(&bound).is_err() {
                Err(PCError::UnsupportedDegreeBound(bound))
            } else if bound < p.degree() || bound > max_degree {
                return Err(PCError::IncorrectDegreeBound {
                    poly_degree: p.degree(),
                    degree_bound: p.degree_bound().unwrap(),
                    supported_degree,
                    label: p.label().to_string(),
                });
            } else {
                Ok(())
            }
        } else {
            Ok(())
        }
    }
}

fn skip_leading_zeros_and_convert_to_bigints<F: PrimeField>(p: &DensePolynomial<F>) -> (usize, Vec<F::BigInteger>) {
    if p.coeffs.is_empty() {
        (0, vec![])
    } else {
        let mut num_leading_zeros = 0;
        while p.coeffs[num_leading_zeros].is_zero() && num_leading_zeros < p.coeffs.len() {
            num_leading_zeros += 1;
        }
        let coeffs = convert_to_bigints(&p.coeffs[num_leading_zeros..]);
        (num_leading_zeros, coeffs)
    }
}

fn convert_to_bigints<F: PrimeField>(p: &[F]) -> Vec<F::BigInteger> {
    let to_bigint_time = start_timer!(|| "Converting polynomial coeffs to bigints");
    let coeffs = cfg_iter!(p).map(|s| s.to_repr()).collect::<Vec<_>>();
    end_timer!(to_bigint_time);
    coeffs
}

#[cfg(test)]
mod tests {
    #![allow(non_camel_case_types)]
    #![allow(clippy::needless_borrow)]
    use super::*;
    use snarkvm_curves::bls12_377::{Bls12_377, Fr};
    use snarkvm_utilities::{rand::TestRng, FromBytes, ToBytes};

    use std::borrow::Cow;

    type KZG_Bls12_377 = KZG10<Bls12_377>;

    impl<E: PairingEngine> KZG10<E> {
        /// Specializes the public parameters for a given maximum degree `d` for polynomials
        /// `d` should be less that `pp.max_degree()`.
        pub(crate) fn trim(pp: &UniversalParams<E>, mut supported_degree: usize) -> (Powers<E>, VerifierKey<E>) {
            if supported_degree == 1 {
                supported_degree += 1;
            }
            let powers_of_beta_g = pp.powers_of_beta_g(0, supported_degree + 1).unwrap().to_vec();
            let powers_of_beta_times_gamma_g =
                (0..=supported_degree).map(|i| pp.get_powers_times_gamma_g()[&i]).collect();

            let powers = Powers {
                powers_of_beta_g: Cow::Owned(powers_of_beta_g),
                powers_of_beta_times_gamma_g: Cow::Owned(powers_of_beta_times_gamma_g),
            };
            let vk = VerifierKey {
                g: pp.power_of_beta_g(0).unwrap(),
                gamma_g: pp.get_powers_times_gamma_g()[&0],
                h: pp.h,
                beta_h: pp.beta_h,
                prepared_h: pp.prepared_h.clone(),
                prepared_beta_h: pp.prepared_beta_h.clone(),
            };
            (powers, vk)
        }
    }

    #[test]
    fn test_kzg10_universal_params_serialization() {
        let rng = &mut TestRng::default();

        let degree = 4;
        let pp = KZG_Bls12_377::setup(degree, &KZG10DegreeBoundsConfig::NONE, false, rng).unwrap();

        let pp_bytes = pp.to_bytes_le().unwrap();
        let pp_recovered: UniversalParams<Bls12_377> = FromBytes::read_le(&pp_bytes[..]).unwrap();
        let pp_recovered_bytes = pp_recovered.to_bytes_le().unwrap();

        assert_eq!(&pp_bytes, &pp_recovered_bytes);
    }

    fn end_to_end_test_template<E: PairingEngine>() -> Result<(), PCError> {
        let rng = &mut TestRng::default();
        for _ in 0..100 {
            let mut degree = 0;
            while degree <= 1 {
                degree = usize::rand(rng) % 20;
            }
            let pp = KZG10::<E>::setup(degree, &KZG10DegreeBoundsConfig::NONE, false, rng)?;
            let (ck, vk) = KZG10::trim(&pp, degree);
            let p = DensePolynomial::rand(degree, rng);
            let hiding_bound = Some(1);
            let (comm, rand) = KZG10::<E>::commit(&ck, &(&p).into(), hiding_bound, &AtomicBool::new(false), Some(rng))?;
            let point = E::Fr::rand(rng);
            let value = p.evaluate(point);
            let proof = KZG10::<E>::open(&ck, &p, point, &rand)?;
            assert!(
                KZG10::<E>::check(&vk, &comm, point, value, &proof)?,
                "proof was incorrect for max_degree = {}, polynomial_degree = {}, hiding_bound = {:?}",
                degree,
                p.degree(),
                hiding_bound,
            );
        }
        Ok(())
    }

    fn linear_polynomial_test_template<E: PairingEngine>() -> Result<(), PCError> {
        let rng = &mut TestRng::default();
        for _ in 0..100 {
            let degree = 50;
            let pp = KZG10::<E>::setup(degree, &KZG10DegreeBoundsConfig::NONE, false, rng)?;
            let (ck, vk) = KZG10::trim(&pp, 2);
            let p = DensePolynomial::rand(1, rng);
            let hiding_bound = Some(1);
            let (comm, rand) = KZG10::<E>::commit(&ck, &(&p).into(), hiding_bound, &AtomicBool::new(false), Some(rng))?;
            let point = E::Fr::rand(rng);
            let value = p.evaluate(point);
            let proof = KZG10::<E>::open(&ck, &p, point, &rand)?;
            assert!(
                KZG10::<E>::check(&vk, &comm, point, value, &proof)?,
                "proof was incorrect for max_degree = {}, polynomial_degree = {}, hiding_bound = {:?}",
                degree,
                p.degree(),
                hiding_bound,
            );
        }
        Ok(())
    }

    fn batch_check_test_template<E: PairingEngine>() -> Result<(), PCError> {
        let rng = &mut TestRng::default();
        for _ in 0..10 {
            let mut degree = 0;
            while degree <= 1 {
                degree = usize::rand(rng) % 20;
            }
            let pp = KZG10::<E>::setup(degree, &KZG10DegreeBoundsConfig::NONE, false, rng)?;
            let (ck, vk) = KZG10::trim(&pp, degree);

            let mut comms = Vec::new();
            let mut values = Vec::new();
            let mut points = Vec::new();
            let mut proofs = Vec::new();

            for _ in 0..10 {
                let p = DensePolynomial::rand(degree, rng);
                let hiding_bound = Some(1);
                let (comm, rand) =
                    KZG10::<E>::commit(&ck, &(&p).into(), hiding_bound, &AtomicBool::new(false), Some(rng))?;
                let point = E::Fr::rand(rng);
                let value = p.evaluate(point);
                let proof = KZG10::<E>::open(&ck, &p, point, &rand)?;

                assert!(KZG10::<E>::check(&vk, &comm, point, value, &proof)?);
                comms.push(comm);
                values.push(value);
                points.push(point);
                proofs.push(proof);
            }
            assert!(KZG10::<E>::batch_check(&vk, &comms, &points, &values, &proofs, rng)?);
        }
        Ok(())
    }

    #[test]
    fn test_end_to_end() {
        end_to_end_test_template::<Bls12_377>().expect("test failed for bls12-377");
    }

    #[test]
    fn test_linear_polynomial() {
        linear_polynomial_test_template::<Bls12_377>().expect("test failed for bls12-377");
    }

    #[test]
    fn test_batch_check() {
        batch_check_test_template::<Bls12_377>().expect("test failed for bls12-377");
    }

    #[test]
    fn test_degree_is_too_large() {
        let rng = &mut TestRng::default();

        let max_degree = 123;
        let pp = KZG_Bls12_377::setup(max_degree, &KZG10DegreeBoundsConfig::NONE, false, rng).unwrap();
        let (powers, _) = KZG_Bls12_377::trim(&pp, max_degree);

        let p = DensePolynomial::<Fr>::rand(max_degree + 1, rng);
        assert!(p.degree() > max_degree);
        assert!(KZG_Bls12_377::check_degree_is_too_large(p.degree(), powers.size()).is_err());
    }
}
