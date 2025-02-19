#[macro_use]
extern crate serde_derive;

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use zokrates_core::ir;
use zokrates_core::{
    compile::{compile, CompileConfig},
    typed_absy::ConcreteType,
};
use zokrates_field::{Bls12_377Field, Bls12_381Field, Bn128Field, Bw6_761Field, Field};
use zokrates_fs_resolver::FileSystemResolver;

#[derive(Serialize, Deserialize, Clone)]
enum Curve {
    Bn128,
    Bls12_381,
    Bls12_377,
    Bw6_761,
}

#[derive(Serialize, Deserialize, Clone)]
struct Tests {
    pub entry_point: Option<PathBuf>,
    pub curves: Option<Vec<Curve>>,
    pub max_constraint_count: Option<usize>,
    pub config: Option<CompileConfig>,
    pub tests: Vec<Test>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Input {
    pub values: Vec<Val>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Test {
    pub abi: Option<bool>,
    pub input: Input,
    pub output: TestResult,
}

type TestResult = Result<Output, ir::Error>;

#[derive(PartialEq, Debug)]
struct ComparableResult<T>(Result<Vec<T>, ir::Error>);

#[derive(Serialize, Deserialize, Clone)]
struct Output {
    values: Vec<Val>,
}

type Val = serde_json::Value;

fn try_parse_raw_val<T: Field>(s: serde_json::Value) -> Result<T, ()> {
    match s {
        serde_json::Value::String(s) => {
            let radix = if s.starts_with("0x") { 16 } else { 10 };
            T::try_from_str(s.trim_start_matches("0x"), radix).map_err(|_| ())
        }
        _ => Err(()),
    }
}

fn try_parse_abi_val<T: Field>(
    s: Vec<Val>,
    types: Vec<ConcreteType>,
) -> Result<Vec<T>, zokrates_abi::Error> {
    use zokrates_abi::Encode;
    zokrates_abi::parse_strict_json(s, types).map(|v| v.encode())
}

impl<T: Field> From<ir::ExecutionResult<T>> for ComparableResult<T> {
    fn from(r: ir::ExecutionResult<T>) -> ComparableResult<T> {
        ComparableResult(r.map(|v| v.return_values()))
    }
}

impl<T: Field> From<TestResult> for ComparableResult<T> {
    fn from(r: TestResult) -> ComparableResult<T> {
        ComparableResult(r.map(|v| {
            v.values
                .into_iter()
                .map(|v| try_parse_raw_val(v).unwrap())
                .collect()
        }))
    }
}

fn compare<T: Field>(result: ir::ExecutionResult<T>, expected: TestResult) -> Result<(), String> {
    // extract outputs from result
    let result = ComparableResult::from(result);
    // deserialize expected result
    let expected = ComparableResult::from(expected);

    if result != expected {
        return Err(format!(
            "Expected {:?} but found {:?}",
            expected.0, result.0
        ));
    }

    Ok(())
}

pub fn test_inner(test_path: &str) {
    let t: Tests =
        serde_json::from_reader(BufReader::new(File::open(Path::new(test_path)).unwrap())).unwrap();

    let curves = t.curves.clone().unwrap_or_else(|| vec![Curve::Bn128]);

    let t = Tests {
        entry_point: Some(
            t.entry_point
                .unwrap_or_else(|| PathBuf::from(String::from(test_path)).with_extension("zok")),
        ),
        ..t
    };

    // this function typically runs in a spawn thread whose stack size is small, leading to stack overflows
    // to avoid that, run the stack-heavy bit in a thread with a larger stack (8M)
    let builder = std::thread::Builder::new().stack_size(8388608);

    builder
        .spawn(move || {
            for c in &curves {
                match c {
                    Curve::Bn128 => compile_and_run::<Bn128Field>(t.clone()),
                    Curve::Bls12_381 => compile_and_run::<Bls12_381Field>(t.clone()),
                    Curve::Bls12_377 => compile_and_run::<Bls12_377Field>(t.clone()),
                    Curve::Bw6_761 => compile_and_run::<Bw6_761Field>(t.clone()),
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

fn compile_and_run<T: Field>(t: Tests) {
    let entry_point = t.entry_point.unwrap();

    let config = t.config.unwrap_or_default();

    let code = std::fs::read_to_string(&entry_point).unwrap();

    let stdlib = std::fs::canonicalize("../zokrates_stdlib/stdlib").unwrap();
    let resolver = FileSystemResolver::with_stdlib_root(stdlib.to_str().unwrap());

    let artifacts = compile::<T, _>(code, entry_point.clone(), Some(&resolver), &config).unwrap();

    let bin = artifacts.prog();
    let abi = artifacts.abi();

    if let Some(target_count) = t.max_constraint_count {
        let count = bin.constraint_count();

        if count > target_count {
            panic!(
                "{} exceeded max constraint count (actual={}, max={}, p={:.2}% of max)",
                entry_point.display(),
                count,
                target_count,
                (count as f32) / (target_count as f32) * 100_f32
            );
        }
    };

    let interpreter = zokrates_core::ir::Interpreter::default();

    for test in t.tests.into_iter() {
        let with_abi = test.abi.unwrap_or(false);

        let input = if with_abi {
            try_parse_abi_val(test.input.values, abi.signature().inputs).unwrap()
        } else {
            test.input
                .values
                .iter()
                .cloned()
                .map(try_parse_raw_val)
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };

        let output = interpreter.execute(bin, &input);

        if let Err(e) = compare(output, test.output) {
            let mut code = File::open(&entry_point).unwrap();
            let mut s = String::new();
            code.read_to_string(&mut s).unwrap();
            let context = format!(
                "\n{}\nCalled on curve {} with input ({})\n",
                s,
                T::name(),
                input
                    .iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            panic!("{}{}", context, e)
        }
    }
}
