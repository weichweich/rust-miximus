extern crate sapling_crypto;
extern crate bellman;
extern crate pairing;
extern crate ff;
extern crate num_bigint;
extern crate num_traits;
extern crate rand;
extern crate time;
extern crate wasm_bindgen;

#[macro_use]
extern crate serde_derive;

use wasm_bindgen::prelude::*;
use rand::{XorShiftRng, SeedableRng};
use bellman::groth16::{Proof, Parameters, verify_proof, create_random_proof, prepare_verifying_key, generate_random_parameters};use num_bigint::BigInt;
use num_traits::Num;
use std::error::Error;

use bellman::{
    Circuit,
    SynthesisError,
    ConstraintSystem,
};

use ff::{Field, PrimeField};
use sapling_crypto::{
    babyjubjub::{
        fs::Fs,
        JubjubEngine,
        JubjubBn256,
    },
    circuit::{
        num::{AllocatedNum},
        baby_pedersen_hash,
        boolean::{Boolean, AllocatedBit}
    }
};

use pairing::{bn256::{Bn256, Fr}};

mod tree;
mod merkle_tree;

/// Circuit for proving knowledge of preimage of leaf in merkle tree
struct MerkleTreeCircuit<'a, E: JubjubEngine> {
    // nullifier
    nullifier: Option<E::Fr>,
    // secret
    secret: Option<E::Fr>,
    root: Option<E::Fr>,
    proof: Vec<Option<(bool, E::Fr)>>,
    params: &'a E::Params,
}

/// Our demo circuit implements this `Circuit` trait which
/// is used during paramgen and proving in order to
/// synthesize the constraint system.
impl<'a, E: JubjubEngine> Circuit<E> for MerkleTreeCircuit<'a, E> {
    fn synthesize<CS: ConstraintSystem<E>>(self, cs: &mut CS) -> Result<(), SynthesisError> {
        // root is public merkle root of the merkle tree
        let root = AllocatedNum::alloc(cs.namespace(|| "root"), || {
            let root_value = self.root.unwrap();
            Ok(root_value)
        })?;
        root.inputize(cs.namespace(|| "public input root"))?;
        // nullifier is the left side of the preimage
        let nullifier = AllocatedNum::alloc(cs.namespace(|| "nullifier"),
            || Ok(match self.nullifier {
                Some(n) => n,
                None => E::Fr::zero(),
            })
        )?;
        nullifier.inputize(cs.namespace(|| "public input nullifier"))?;
        // secret is the right side of the preimage
        let secret = AllocatedNum::alloc(cs.namespace(|| "secret"),
            || Ok(match self.secret {
                Some(s) => s,
                None => E::Fr::zero(),
            })
        )?;
        // construct preimage using [nullifier_bits|secret_bits] concatenation
        let nullifier_bits = nullifier.into_bits_le_strict(cs.namespace(|| "nullifier bits")).unwrap().into_iter().take(Fr::NUM_BITS as usize);
        let secret_bits = secret.into_bits_le_strict(cs.namespace(|| "secret bits")).unwrap().into_iter().take(Fr::NUM_BITS as usize);
        let mut preimage = vec![];
        preimage.extend(nullifier_bits);
        preimage.extend(secret_bits);
        // compute leaf hash using pedersen hash of preimage
        let mut hash = baby_pedersen_hash::pedersen_hash(
            cs.namespace(|| "computation of leaf pedersen hash"),
            baby_pedersen_hash::Personalization::NoteCommitment,
            &preimage,
            self.params
        )?.get_x().clone();
        // reconstruct merkle root hash using the private merkle path
        for i in 0..self.proof.len() {
			match self.proof[i] {
                Some((ref side, ref element)) => {
                    let elt = AllocatedNum::alloc(cs.namespace(|| format!("elt {}", i)), || Ok(*element))?;
                    let right_side = Boolean::from(AllocatedBit::alloc(
                        cs.namespace(|| format!("position bit {}", i)),
                        Some(*side)).unwrap()
                    );
                    // Swap the two if the current subtree is on the right
                    let (xl, xr) = AllocatedNum::conditionally_reverse(
                        cs.namespace(|| format!("conditional reversal of preimage {}", i)),
                        &elt,
                        &hash,
                        &right_side
                    )?;

                    let mut preimage = vec![];
                    preimage.extend(xl.into_bits_le(cs.namespace(|| format!("xl into bits {}", i)))?);
                    preimage.extend(xr.into_bits_le(cs.namespace(|| format!("xr into bits {}", i)))?);

                    // Compute the new subtree value
                    hash = baby_pedersen_hash::pedersen_hash(
                        cs.namespace(|| format!("computation of pedersen hash {}", i)),
                        baby_pedersen_hash::Personalization::MerkleTree(i as usize),
                        &preimage,
                        self.params
                    )?.get_x().clone(); // Injective encoding
                },
                None => (),
            }
        }

        cs.enforce(
            || "enforce new root equal to recalculated one",
            |lc| lc + hash.get_variable(),
            |lc| lc + CS::one(),
            |lc| lc + root.get_variable()
        );

        Ok(())
    }
}

#[wasm_bindgen]
extern "C" {
    fn alert(s: &str);
}

#[derive(Serialize)]
pub struct KGGenerate {
    pub params: String
}

#[derive(Serialize)]
pub struct KGProof {
    pub proof: String,
    // pub nullifier: String,
    // pub secret: String,
    // pub leaf: String,
    // pub path: Vec<String>
}

#[derive(Serialize)]
pub struct KGVerify {
    pub result: bool
}

#[wasm_bindgen(catch)]
pub fn generate(seed_slice: &[u32]) -> Result<JsValue, JsValue> {
    let res = || -> Result<JsValue, Box<Error>> {
        let mut seed : [u32; 4] = [0; 4];
        seed.copy_from_slice(seed_slice);
        let rng = &mut XorShiftRng::from_seed(seed);

        let j_params = &JubjubBn256::new();
        let params = generate_random_parameters::<Bn256, _, _>(
            MerkleTreeCircuit {
                params: j_params,
                nullifier: None,
                secret: None,
                root: None,
                proof: vec![],
            },
            rng,
        )?;

        let mut v = vec![];

        params.write(&mut v)?;

        Ok(JsValue::from_serde(&KGGenerate {
            params: hex::encode(&v[..])
        })?)
    }();
    convert_error_to_jsvalue(res)
}

#[wasm_bindgen(catch)]
pub fn prove(
        seed_slice: &[u32],
        params: &str,
        nullifier_hex: &str,
        secret_hex: &str,
        root_hex: &str,
        proof_path_hex: &str,
        proof_path_sides: &str,
) -> Result<JsValue, JsValue> {
    let res = || -> Result<JsValue, Box<Error>> {
        if params.len() == 0 {
            return Err("Params are empty. Did you generate or load params?".into())
        }
        let de_params = Parameters::<Bn256>::read(&hex::decode(params)?[..], true)?;
        let j_params = &JubjubBn256::new();

        let mut seed : [u32; 4] = [0; 4];
        seed.copy_from_slice(seed_slice);
        let rng = &mut XorShiftRng::from_seed(seed);
        let params = &JubjubBn256::new();

        let s = &format!("{}", Fs::char())[2..];
        let s_big = BigInt::from_str_radix(s, 16)?;
        // Nullifier
        let nullifier_big = BigInt::from_str_radix(nullifier_hex, 16)?;
        if nullifier_big >= s_big {
            return Err("nullifier should be less than 60c89ce5c263405370a08b6d0302b0bab3eedb83920ee0a677297dc392126f1".into())
        }
        let nullifier_raw = &nullifier_big.to_str_radix(10);
        let nullifier = Fr::from_str(nullifier_raw).ok_or("couldn't parse Fr")?;
        let nullifier_s = Fr::from_str(nullifier_raw).ok_or("couldn't parse Fr")?;
        // Secret preimage data
        let secret_big = BigInt::from_str_radix(secret_hex, 16)?;
        if secret_big >= s_big {
            return Err("secret should be less than 60c89ce5c263405370a08b6d0302b0bab3eedb83920ee0a677297dc392126f1".into())
        }
        let secret_raw = &secret_big.to_str_radix(10);
        let secret = Fr::from_str(secret_raw).ok_or("couldn't parse Fr")?;
        let secret_s = Fr::from_str(secret_raw).ok_or("couldn't parse Fr")?;
        // Root hash
        let root_big = BigInt::from_str_radix(root_hex, 16)?;
        if root_big >= s_big {
            return Err("root should be less than 60c89ce5c263405370a08b6d0302b0bab3eedb83920ee0a677297dc392126f1".into())
        }
        let root_raw = &root_big.to_str_radix(10);
        let root = Fr::from_str(root_raw).ok_or("couldn't parse Fr")?;
        let root_s = Fr::from_str(root_raw).ok_or("couldn't parse Fr")?;
        // Proof path
        let mut proof_p_big: Vec<Option<(bool, pairing::bn256::Fr)>> = vec![];
        let proof_len = proof_path_hex.len();
        let depth = proof_len / 32;
        for i in 0..depth {
            let (neighbor_i, proof_path_hex) = proof_path_hex.split_at(32);
            let (side_i, proof_path_sides) = proof_path_sides.split_at(1);
            let mut side_bool = false;
            if side_i == "1" {
                side_bool = true;
            }

            let p_big = BigInt::from_str_radix(neighbor_i, 16)?;
            if p_big >= s_big {
                return Err("root should be less than 60c89ce5c263405370a08b6d0302b0bab3eedb83920ee0a677297dc392126f1".into())
            }
            let p_raw = &p_big.to_str_radix(10);
            let p = Fr::from_str(p_raw).ok_or("couldn't parse Fr")?;
            let p_s = Fr::from_str(p_raw).ok_or("couldn't parse Fr")?;
            proof_p_big.push(Some((
                side_bool,
                p,
            )));
        }

        let proof = create_random_proof(
            MerkleTreeCircuit {
                params: j_params,
                nullifier: Some(nullifier),
                secret: Some(secret),
                root: Some(root),
                proof: vec![],
            },
            &de_params,
            rng
        )?;

        let mut v = vec![];
        proof.write(&mut v)?;

        Ok(JsValue::from_serde(&KGProof {
            proof: hex::encode(&v[..]),
        })?)
    }();

    convert_error_to_jsvalue(res)
}

#[wasm_bindgen(catch)]
pub fn verify(params: &str, proof: &str, nullifier_hex: &str, root_hex: &str) -> Result<JsValue, JsValue> {
    let res = || -> Result<JsValue, Box<Error>> {
        let de_params = Parameters::read(&hex::decode(params)?[..], true)?;
        let j_params = &JubjubBn256::new();
        let pvk = prepare_verifying_key::<Bn256>(&de_params.vk);


        let s = &format!("{}", Fs::char())[2..];
        let s_big = BigInt::from_str_radix(s, 16)?;
        // Nullifier
        let nullifier_big = BigInt::from_str_radix(nullifier_hex, 16)?;
        if nullifier_big >= s_big {
            return Err("x should be less than 60c89ce5c263405370a08b6d0302b0bab3eedb83920ee0a677297dc392126f1".into())
        }
        let nullifier_raw = &nullifier_big.to_str_radix(10);
        let nullifier = Fr::from_str(nullifier_raw).ok_or("couldn't parse Fr")?;
        let nullifier_s = Fr::from_str(nullifier_raw).ok_or("couldn't parse Fr")?;
        // Root hash
        let root_big = BigInt::from_str_radix(root_hex, 16)?;
        if root_big >= s_big {
            return Err("x should be less than 60c89ce5c263405370a08b6d0302b0bab3eedb83920ee0a677297dc392126f1".into())
        }
        let root_raw = &root_big.to_str_radix(10);
        let root = Fr::from_str(root_raw).ok_or("couldn't parse Fr")?;
        let root_s = Fr::from_str(root_raw).ok_or("couldn't parse Fr")?;


        let result = verify_proof(
            &pvk,
            &Proof::read(&hex::decode(proof)?[..])?,
            &[
                nullifier,
                root
            ])?;

        Ok(JsValue::from_serde(&KGVerify{
            result: result
        })?)
    }();
    convert_error_to_jsvalue(res)
}

fn convert_error_to_jsvalue(res: Result<JsValue, Box<Error>>) -> Result<JsValue, JsValue> {
    if res.is_ok() {
        Ok(res.ok().unwrap())
    } else {
        Err(JsValue::from_str(&res.err().unwrap().to_string()))
    }
}

#[cfg(test)]
mod test {
    use pairing::{bn256::{Bn256, Fr}};
    use sapling_crypto::{
        babyjubjub::{
            JubjubBn256,
            JubjubEngine,
        }
    };
    use rand::{XorShiftRng, SeedableRng};

    use sapling_crypto::circuit::{
        test::TestConstraintSystem
    };
    use bellman::{
        Circuit,
    };
    use rand::Rand;

    use super::MerkleTreeCircuit;
    use merkle_tree::{create_leaf_list, create_leaf_from_preimage, build_merkle_tree_with_proof};
    use time::PreciseTime;

    #[test]
    fn test_merkle_circuit() {
        let mut cs = TestConstraintSystem::<Bn256>::new();
        let mut seed : [u32; 4] = [0; 4];
        seed.copy_from_slice(&[1u32, 1u32, 1u32, 1u32]);
        let rng = &mut XorShiftRng::from_seed(seed);
        println!("generating setup...");
        let start = PreciseTime::now();
        
        let mut proof_vec = vec![];
        for _ in 0..32 {
            proof_vec.push(Some((
                true,
                Fr::rand(rng))
            ));
        }

        let j_params = &JubjubBn256::new();
        let m_circuit = MerkleTreeCircuit {
            params: j_params,
            nullifier: Some(Fr::rand(rng)),
            secret: Some(Fr::rand(rng)),
            root: Some(Fr::rand(rng)),
            proof: proof_vec,
        };

        m_circuit.synthesize(&mut cs).unwrap();
        println!("setup generated in {} s", start.to(PreciseTime::now()).num_milliseconds() as f64 / 1000.0);
        println!("num constraints: {}", cs.num_constraints());
        println!("num inputs: {}", cs.num_inputs());
    }

    fn test_wasm_fns() {
        let mut cs = TestConstraintSystem::<Bn256>::new();
        let mut seed : [u32; 4] = [0; 4];
        seed.copy_from_slice(&[1u32, 1u32, 1u32, 1u32]);
        let rng = &mut XorShiftRng::from_seed(seed);
        println!("generating setup...");
        let start = PreciseTime::now();
        
        let nullifier = Fr::rand(rng);
        let secret = Fr::rand(rng);
        let leaf = create_leaf_from_preimage(nullifier, secret);

        let mut leaves = vec![*leaf.hash()];
        for i in 0..7 {
            leaves.push(Fr::rand(rng));
        }
        let tree_nodes = create_leaf_list(leaves, 3);
        let (_r, proof_vec) = build_merkle_tree_with_proof(tree_nodes, 3, *leaf.hash(), vec![]);

        let j_params = &JubjubBn256::new();
        let m_circuit = MerkleTreeCircuit {
            params: j_params,
            nullifier: Some(nullifier),
            secret: Some(secret),
            root: Some(*_r.root.hash()),
            proof: proof_vec,
        };
        m_circuit.synthesize(&mut cs).unwrap();
        println!("setup generated in {} s", start.to(PreciseTime::now()).num_milliseconds() as f64 / 1000.0);
        println!("num constraints: {}", cs.num_constraints());
        println!("num inputs: {}", cs.num_inputs());
    }
}