mod config;
mod generator;
mod metrics;
mod runner;

use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context};
use config::Profile;
use runner::{BenchmarkRunner, BinaryPaths};
use serde::Serialize;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse()?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .context("benchmark crate must live below benchmarks/")?
        .to_owned();
    let profile_path = cli.profile.unwrap_or_else(|| {
        root.join("benchmarks/kas-benchmark/profiles")
            .join(match cli.command.as_str() {
                "sweep" => "scale.json",
                "find-limit" => "limit.json",
                _ => "smoke.json",
            })
    });
    let profile = Profile::read(&profile_path)?;
    let output = cli.output.unwrap_or_else(|| root.join("benchmark-results"));
    fs::create_dir_all(&output)?;
    let bin_dir = cli.bin_dir.unwrap_or_else(|| {
        env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_owned))
            .unwrap_or_else(|| root.join("target/release"))
    });
    let runner = BenchmarkRunner::new(root, BinaryPaths::from_directory(&bin_dir), output.clone());

    match cli.command.as_str() {
        "run" | "smoke" => {
            let (result, directory) = runner
                .run(
                    &profile.name,
                    profile.scenario.clone(),
                    &profile,
                    &profile.name,
                )
                .await?;
            println!(
                "{}: {} ({})",
                profile.name,
                if result.passed { "PASS" } else { "FAIL" },
                directory.display()
            );
            if !result.passed {
                bail!("benchmark violated its service-level objectives");
            }
        }
        "sweep" => {
            let mut failed = false;
            for (dimension, values) in &profile.sweeps {
                if cli
                    .dimension
                    .as_ref()
                    .is_some_and(|selected| selected != dimension)
                {
                    continue;
                }
                for value in values {
                    let mut scenario = profile.scenario.clone();
                    scenario.set_dimension(dimension, value)?;
                    let run_name = format!(
                        "{}-{}",
                        dimension,
                        value
                            .as_u64()
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "value".into())
                    );
                    let (result, directory) = runner
                        .run(&profile.name, scenario, &profile, &run_name)
                        .await?;
                    println!(
                        "{run_name}: {} ({})",
                        if result.passed { "PASS" } else { "FAIL" },
                        directory.display()
                    );
                    failed |= !result.passed;
                }
            }
            if failed {
                bail!("one or more benchmark sweep points violated their SLO");
            }
        }
        "find-limit" => {
            let dimension = cli
                .dimension
                .unwrap_or_else(|| profile.limit.dimension.clone());
            let start = cli.start.unwrap_or(profile.limit.start);
            let max = cli.max.unwrap_or(profile.limit.max);
            let summary = find_limit(&runner, &profile, &dimension, start, max).await;
            let path = output.join(format!(
                "limit-{}-{}.json",
                dimension,
                chrono::Utc::now().format("%Y%m%dT%H%M%S")
            ));
            fs::write(&path, serde_json::to_vec_pretty(&summary)?)?;
            println!(
                "{} limit: last_good={:?}, first_bad={:?} ({})",
                dimension,
                summary.last_good,
                summary.first_bad,
                path.display()
            );
        }
        other => bail!("unknown command {other}; use run, smoke, sweep, or find-limit"),
    }
    Ok(())
}

async fn find_limit(
    runner: &BenchmarkRunner,
    profile: &Profile,
    dimension: &str,
    start: u64,
    max: u64,
) -> LimitSummary {
    let mut summary = LimitSummary {
        dimension: dimension.into(),
        last_good: None,
        first_bad: None,
        attempts: BTreeMap::new(),
    };
    let mut value = start.max(1);
    while value <= max {
        let attempt = run_limit_point(runner, profile, dimension, value).await;
        let passed = attempt.passed;
        summary.attempts.insert(value, attempt);
        if passed {
            summary.last_good = Some(value);
            let next = value.saturating_mul(profile.limit.multiplier.max(2));
            if next <= value {
                break;
            }
            value = next;
        } else {
            summary.first_bad = Some(value);
            break;
        }
    }
    if summary.first_bad.is_none() && summary.last_good.is_some_and(|good| good < max) {
        let attempt = run_limit_point(runner, profile, dimension, max).await;
        let passed = attempt.passed;
        summary.attempts.insert(max, attempt);
        if passed {
            summary.last_good = Some(max);
        } else {
            summary.first_bad = Some(max);
        }
    }
    if let (Some(mut good), Some(mut bad)) = (summary.last_good, summary.first_bad) {
        while bad.saturating_sub(good) > 1 {
            let middle = good + (bad - good) / 2;
            let attempt = run_limit_point(runner, profile, dimension, middle).await;
            let passed = attempt.passed;
            summary.attempts.insert(middle, attempt);
            if passed {
                good = middle;
            } else {
                bad = middle;
            }
        }
        summary.last_good = Some(good);
        summary.first_bad = Some(bad);
    }
    summary
}

async fn run_limit_point(
    runner: &BenchmarkRunner,
    profile: &Profile,
    dimension: &str,
    value: u64,
) -> LimitAttempt {
    let repetitions = profile.limit.repetitions.max(1);
    let required_passes = profile.limit.required_passes.clamp(1, repetitions);
    let mut passes = 0;
    let mut errors = Vec::new();
    for repetition in 1..=repetitions {
        let mut scenario = profile.scenario.clone();
        if let Err(error) = scenario.set_dimension_u64(dimension, value) {
            errors.push(error.to_string());
            break;
        }
        let name = format!("limit-{dimension}-{value}-rep{repetition}");
        match runner.run(&profile.name, scenario, profile, &name).await {
            Ok((result, directory)) => {
                println!(
                    "{name}: {} ({})",
                    if result.passed { "PASS" } else { "FAIL" },
                    directory.display()
                );
                passes += usize::from(result.passed);
            }
            Err(error) => {
                eprintln!("{name}: ERROR: {error:#}");
                errors.push(format!("{error:#}"));
            }
        }
    }
    LimitAttempt {
        passed: passes >= required_passes,
        passes,
        repetitions,
        required_passes,
        errors,
    }
}

#[derive(Serialize)]
struct LimitSummary {
    dimension: String,
    last_good: Option<u64>,
    first_bad: Option<u64>,
    attempts: BTreeMap<u64, LimitAttempt>,
}

#[derive(Serialize)]
struct LimitAttempt {
    passed: bool,
    passes: usize,
    repetitions: usize,
    required_passes: usize,
    errors: Vec<String>,
}

struct Cli {
    command: String,
    profile: Option<PathBuf>,
    output: Option<PathBuf>,
    bin_dir: Option<PathBuf>,
    dimension: Option<String>,
    start: Option<u64>,
    max: Option<u64>,
}

impl Cli {
    fn parse() -> anyhow::Result<Self> {
        let mut arguments = env::args().skip(1);
        let command = arguments.next().unwrap_or_else(|| "smoke".into());
        let mut cli = Self {
            command,
            profile: None,
            output: None,
            bin_dir: None,
            dimension: None,
            start: None,
            max: None,
        };
        while let Some(argument) = arguments.next() {
            let value = arguments
                .next()
                .with_context(|| format!("{argument} requires a value"))?;
            match argument.as_str() {
                "--profile" => cli.profile = Some(value.into()),
                "--output" => cli.output = Some(value.into()),
                "--bin-dir" => cli.bin_dir = Some(value.into()),
                "--dimension" => cli.dimension = Some(value),
                "--start" => cli.start = Some(value.parse()?),
                "--max" => cli.max = Some(value.parse()?),
                other => bail!("unknown option {other}"),
            }
        }
        Ok(cli)
    }
}
