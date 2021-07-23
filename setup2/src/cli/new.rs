use phase2::parameters::{circuit_to_qap, MPCParameters};
use setup_utils::{log_2, CheckForCorrectness, Groth16Params, UseCompression};
use snarkvm_algorithms::{MerkleParameters, CRH, SNARK};
use snarkvm_curves::{PairingEngine};
use snarkvm_dpc::testnet1::{
    inner_circuit::InnerCircuit,
    instantiated::{
        CommitmentMerkleParameters,
        Components,
        InnerPairing,
        InstantiatedDPC,
        MerkleTreeCRH,
        OuterPairing,
    },
    outer_circuit::OuterCircuit,
    parameters::SystemParameters,
    program::PrivateProgramInput,
    BaseDPCComponents,
    NoopCircuit,
};
use snarkvm_fields::Field;
use snarkvm_parameters::{traits::Parameter, LedgerMerkleTreeParameters};
use snarkvm_r1cs::{ConstraintCounter, ConstraintSynthesizer};
use snarkvm_utilities::{
    bytes::{FromBytes, ToBytes},
    to_bytes,
};

use gumdrop::Options;
use memmap::MmapOptions;
use rand::SeedableRng;
use rand_chacha::ChaChaRng;
use std::fs::OpenOptions;
use std::sync::Arc;

type AleoInner = InnerPairing;
type AleoOuter = OuterPairing;

const COMPRESSION: UseCompression = UseCompression::No;

#[derive(Debug, Clone)]
pub enum CurveKind {
    Bls12_377,
    BW6,
}

pub fn curve_from_str(src: &str) -> std::result::Result<CurveKind, String> {
    let curve = match src.to_lowercase().as_str() {
        "bls12_377" => CurveKind::Bls12_377,
        "bw6" => CurveKind::BW6,
        _ => return Err("unsupported curve.".to_string()),
    };
    Ok(curve)
}

#[derive(Debug, Options, Clone)]
pub struct NewOpts {
    help: bool,
    #[options(help = "the path to the phase1 parameters", default = "phase1")]
    pub phase1: String,
    #[options(help = "the total number of coefficients (in powers of 2) which were created after processing phase 1")]
    pub phase1_size: u32,
    #[options(help = "the challenge file name to be created", default = "challenge")]
    pub output: String,

    #[options(
        help = "the elliptic curve to use",
        default = "bls12_377",
        parse(try_from_str = "curve_from_str")
    )]
    pub curve_type: CurveKind,

    #[options(help = "setup the inner or the outer circuit?")]
    pub is_inner: bool,
}

pub fn new(opt: &NewOpts) -> anyhow::Result<()> {
    let circuit_parameters = SystemParameters::<Components>::load()?;

    // Load the inner circuit & merkle params
    let params_bytes = LedgerMerkleTreeParameters::load_bytes()?;
    let params = <MerkleTreeCRH as CRH>::Parameters::read(&params_bytes[..])?;
    let merkle_tree_hash_parameters = <CommitmentMerkleParameters as MerkleParameters>::H::from(params);
    let merkle_params = Arc::new(From::from(merkle_tree_hash_parameters));

    if opt.is_inner {
        let circuit = InnerCircuit::blank(&circuit_parameters, &merkle_params);
        generate_params::<AleoInner, _>(opt, circuit)
    } else {
        let rng = &mut ChaChaRng::from_seed([0u8; 32]);
        let noop_program_snark_parameters =
            InstantiatedDPC::generate_noop_program_snark_parameters(&circuit_parameters, rng)?;
        let program_snark_proof = <Components as BaseDPCComponents>::NoopProgramSNARK::prove(
            &noop_program_snark_parameters.proving_key,
            &NoopCircuit::<Components>::blank(&circuit_parameters),
            rng,
        )?;

        let private_program_input = PrivateProgramInput {
            verification_key: to_bytes![noop_program_snark_parameters.verification_key.clone()]?,
            proof: to_bytes![program_snark_proof]?,
        };

        let inner_snark_parameters = <Components as BaseDPCComponents>::InnerSNARK::setup(
            &InnerCircuit::blank(&circuit_parameters, &merkle_params),
            rng,
        )?;

        let inner_snark_vk: <<Components as BaseDPCComponents>::InnerSNARK as SNARK>::VerifyingKey =
            inner_snark_parameters.1.clone().into();
        let inner_snark_proof = <Components as BaseDPCComponents>::InnerSNARK::prove(
            &inner_snark_parameters.0,
            &InnerCircuit::blank(&circuit_parameters, &merkle_params),
            rng,
        )?;

        let circuit = OuterCircuit::blank(
            circuit_parameters,
            merkle_params,
            inner_snark_vk,
            inner_snark_proof,
            private_program_input,
        );
        generate_params::<AleoOuter, _>(opt, circuit)
    }
}

/// Returns the number of powers required for the Phase 2 ceremony
/// = log2(aux + inputs + constraints)
fn ceremony_size<F: Field, C: Clone + ConstraintSynthesizer<F>>(circuit: &C) -> usize {
    let mut counter = ConstraintCounter{
        num_constraints: 0,
        num_private_variables: 0,
        num_public_variables: 0,
    };
    circuit
        .clone()
        .generate_constraints(&mut counter)
        .expect("could not calculate number of required constraints");
    let phase2_size = std::cmp::max(counter.num_constraints, counter.num_private_variables + counter.num_public_variables + 1);
    let power = log_2(phase2_size) as u32;

    // get the nearest power of 2
    if phase2_size < 2usize.pow(power) {
        2usize.pow(power + 1)
    } else {
        phase2_size
    }
}

pub fn generate_params<E: PairingEngine, C: Clone + ConstraintSynthesizer<E::Fr>>(
    opt: &NewOpts,
    circuit: C,
) -> anyhow::Result<()> {
    let phase1_transcript = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&opt.phase1)
        .expect("could not read phase 1 transcript file");
    let mut phase1_transcript = unsafe {
        MmapOptions::new()
            .map_mut(&phase1_transcript)
            .expect("unable to create a memory map for input")
    };
    let mut output = OpenOptions::new()
        .read(false)
        .write(true)
        .create_new(true)
        .open(&opt.output)
        .expect("could not open file for writing the MPC parameters ");

    let phase2_size = ceremony_size(&circuit);
    let keypair = circuit_to_qap::<E, _>(circuit)?;

    // Read `num_constraints` Lagrange coefficients from the Phase1 Powers of Tau which were
    // prepared for this step. This will fail if Phase 1 was too small.
    let phase1 = Groth16Params::<E>::read(
        &mut phase1_transcript,
        COMPRESSION,
        CheckForCorrectness::No, // No need to check for correctness, since this has been processed by the coordinator.
        2usize.pow(opt.phase1_size),
        phase2_size,
    )?;

    // Generate the initial transcript
    let mpc = MPCParameters::new(keypair, phase1)?;
    mpc.write(&mut output)?;

    Ok(())
}
