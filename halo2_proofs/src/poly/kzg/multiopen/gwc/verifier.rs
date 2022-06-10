use std::fmt::Debug;
use std::io::Read;
use std::marker::PhantomData;

use super::{construct_intermediate_sets, ChallengeU, ChallengeV};
use crate::arithmetic::{eval_polynomial, lagrange_interpolate, CurveAffine, FieldExt};

use crate::poly::commitment::Verifier;
use crate::poly::commitment::MSM;
use crate::poly::kzg::commitment::{KZGCommitmentScheme, ParamsKZG};
use crate::poly::kzg::msm::{DualMSM, MSMKZG};
use crate::poly::kzg::strategy::{BatchVerifier, GuardKZG};
use crate::poly::query::Query;
use crate::poly::query::{CommitmentReference, VerifierQuery};
use crate::poly::strategy::VerificationStrategy;
use crate::poly::{
    commitment::{Params, ParamsVerifier},
    Error,
};
use crate::transcript::{EncodedChallenge, TranscriptRead};

use ff::Field;
use group::Group;
use halo2curves::pairing::{Engine, MillerLoopResult, MultiMillerLoop};
use rand_core::RngCore;

#[derive(Debug)]
/// Concrete KZG verifier with GWC variant
pub struct VerifierGWC<'params, E: Engine> {
    params: &'params ParamsKZG<E>,
}

impl<'params, E: MultiMillerLoop + Debug> Verifier<'params, KZGCommitmentScheme<E>>
    for VerifierGWC<'params, E>
{
    type Guard = GuardKZG<'params, E>;
    type MSMAccumulator = DualMSM<'params, E>;

    fn new(params: &'params ParamsKZG<E>) -> Self {
        Self { params }
    }

    fn verify_proof<
        'com,
        Ch: EncodedChallenge<E::G1Affine>,
        T: TranscriptRead<E::G1Affine, Ch>,
        I,
    >(
        &self,
        transcript: &mut T,
        queries: I,
        mut msm_accumulator: DualMSM<'params, E>,
    ) -> Result<Self::Guard, Error>
    where
        I: IntoIterator<Item = VerifierQuery<'com, E::G1Affine>> + Clone,
    {
        let v: ChallengeV<_> = transcript.squeeze_challenge_scalar();

        let commitment_data = construct_intermediate_sets(queries);

        let w: Vec<E::G1Affine> = (0..commitment_data.len())
            .map(|_| transcript.read_point().map_err(|_| Error::SamplingError))
            .collect::<Result<Vec<E::G1Affine>, Error>>()?;

        let u: ChallengeU<_> = transcript.squeeze_challenge_scalar();

        let mut commitment_multi = MSMKZG::<E>::new();
        let mut eval_multi = E::Scalar::zero();

        let mut witness = MSMKZG::<E>::new();
        let mut witness_with_aux = MSMKZG::<E>::new();

        for (commitment_at_a_point, wi) in commitment_data.iter().zip(w.into_iter()) {
            assert!(!commitment_at_a_point.queries.is_empty());
            let z = commitment_at_a_point.point;

            witness_with_aux.scale(*u);
            witness_with_aux.append_term(z, wi.into());
            witness.scale(*u);
            witness.append_term(E::Scalar::one(), wi.into());
            commitment_multi.scale(*u);
            eval_multi = eval_multi * *u;

            let mut commitment_batch = MSMKZG::<E>::new();
            let mut eval_batch = E::Scalar::zero();

            for query in commitment_at_a_point.queries.iter() {
                assert_eq!(query.get_point(), z);

                let commitment = query.get_commitment();
                let eval = query.get_eval();

                commitment_batch.scale(*v);
                match commitment {
                    CommitmentReference::Commitment(c) => {
                        commitment_batch.append_term(E::Scalar::one(), (*c).into());
                    }
                    CommitmentReference::MSM(msm) => {
                        commitment_batch.add_msm(msm);
                    }
                }

                eval_batch = eval_batch * *v + eval;
            }

            commitment_multi.add_msm(&commitment_batch);
            eval_multi += eval_batch;
        }

        msm_accumulator.left.add_msm(&witness);

        msm_accumulator.right.add_msm(&witness_with_aux);
        msm_accumulator.right.add_msm(&commitment_multi);
        let g0: E::G1 = self.params.g[0].into();
        msm_accumulator.right.append_term(eval_multi, -g0);

        Ok(Self::Guard::new(msm_accumulator))
    }
}

impl<'params, E: MultiMillerLoop + Debug, R: RngCore>
    VerificationStrategy<'params, KZGCommitmentScheme<E>, VerifierGWC<'params, E>, R>
    for BatchVerifier<'params, E, R>
{
    type Output = Self;

    fn new(params: &'params ParamsKZG<E>, rng: R) -> Self {
        BatchVerifier::new(params, rng)
    }

    fn process(
        mut self,
        f: impl FnOnce(DualMSM<'params, E>) -> Result<GuardKZG<'params, E>, crate::plonk::Error>,
    ) -> Result<Self::Output, crate::plonk::Error> {
        self.msm_accumulator.scale(E::Scalar::random(&mut self.rng));

        // Guard is updated with new msm contributions
        let guard = f(self.msm_accumulator)?;
        Ok(BatchVerifier::with(guard.msm_accumulator, self.rng))
    }

    fn finalize(self) -> bool {
        self.msm_accumulator.check()
    }
}