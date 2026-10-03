//! Reference-Windows GPU pixel proof for compositor VerticalFlow placement.
//!
//! The binary uses the real `SceneGraph` -> `HeadlessRuntime` -> compositor
//! production render path. It writes a PPM plus a fail-closed JSON verdict; a
//! missing hardware tag, wrong surface, absent sample, overlap, or glyph/backdrop
//! y mismatch makes the process exit non-zero.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use render_artifacts::vertical_flow_proof::{
    ContractCheck, ProofEvidence, ProofReport, ProofVerdict, REFERENCE_HEIGHT, REFERENCE_WIDTH,
    ReadbackDimensions, ReferenceHardware, RendererIdentity, VerticalFlowFixture, build_fixture,
    evaluate_evidence, observe_pixels, reference_preflight_passes,
};
use tze_hud_compositor::CompositorSurface;
use tze_hud_runtime::headless::{HeadlessConfig, HeadlessRuntime};

const REPORT_FILE: &str = "vertical-flow-readback.json";
const FRAME_FILE: &str = "vertical-flow-readback.ppm";

#[derive(Debug, PartialEq)]
struct Args {
    output_dir: PathBuf,
    reference_hardware_tag: String,
    reference_hostname: String,
    reference_gpu: String,
    reference_gpu_driver: String,
    reference_os: String,
    display_width: u32,
    display_height: u32,
}

fn parse_args_from(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args {
        output_dir: PathBuf::from("test_results/vertical-flow-readback"),
        reference_hardware_tag: String::new(),
        reference_hostname: String::new(),
        reference_gpu: String::new(),
        reference_gpu_driver: String::new(),
        reference_os: String::new(),
        display_width: 0,
        display_height: 0,
    };
    let args = args.into_iter().collect::<Vec<_>>();
    let mut index = 0;
    while index < args.len() {
        let flag = &args[index];
        if flag == "--help" || flag == "-h" {
            return Err(usage().to_string());
        }
        index += 1;
        let value = args
            .get(index)
            .ok_or_else(|| format!("missing value for {flag}\n{}", usage()))?;
        match flag.as_str() {
            "--output" | "-o" => parsed.output_dir = PathBuf::from(value),
            "--reference-hardware-tag" => parsed.reference_hardware_tag = value.clone(),
            "--reference-hostname" => parsed.reference_hostname = value.clone(),
            "--reference-gpu" => parsed.reference_gpu = value.clone(),
            "--reference-gpu-driver" => parsed.reference_gpu_driver = value.clone(),
            "--reference-os" => parsed.reference_os = value.clone(),
            "--display-width" => {
                parsed.display_width = value
                    .parse()
                    .map_err(|_| format!("invalid --display-width {value:?}"))?;
            }
            "--display-height" => {
                parsed.display_height = value
                    .parse()
                    .map_err(|_| format!("invalid --display-height {value:?}"))?;
            }
            _ => return Err(format!("unknown option {flag:?}\n{}", usage())),
        }
        index += 1;
    }
    Ok(parsed)
}

fn usage() -> &'static str {
    "Usage: vertical-flow-readback [--output DIR] \
--reference-hardware-tag TAG --reference-hostname HOST \
--reference-gpu GPU --reference-gpu-driver DRIVER --reference-os OS \
--display-width PX --display-height PX"
}

impl Args {
    fn reference_hardware(&self) -> ReferenceHardware {
        ReferenceHardware {
            tag: self.reference_hardware_tag.clone(),
            hostname: self.reference_hostname.clone(),
            gpu: self.reference_gpu.clone(),
            gpu_driver: self.reference_gpu_driver.clone(),
            os: self.reference_os.clone(),
            display_width: self.display_width,
            display_height: self.display_height,
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let args = match parse_args_from(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = fs::create_dir_all(&args.output_dir) {
        eprintln!("unable to create {}: {error}", args.output_dir.display());
        std::process::exit(2);
    }

    let fixture = match build_fixture() {
        Ok(fixture) => fixture,
        Err(error) => {
            eprintln!("unable to build proof fixture: {error}");
            std::process::exit(2);
        }
    };
    let reference = args.reference_hardware();
    let preflight = empty_report(reference.clone(), &fixture);
    if !reference_preflight_passes(&preflight) {
        finish(&args.output_dir, &preflight);
    }

    let report = match capture(reference.clone(), &fixture).await {
        Ok((report, pixels)) => {
            if pixels.len() == (REFERENCE_WIDTH * REFERENCE_HEIGHT * 4) as usize {
                if let Err(error) = write_ppm(
                    &args.output_dir.join(FRAME_FILE),
                    REFERENCE_WIDTH,
                    REFERENCE_HEIGHT,
                    &pixels,
                ) {
                    runtime_failure_report(
                        reference,
                        &fixture,
                        "artifact_write",
                        format!("unable to write PPM: {error}"),
                    )
                } else {
                    report
                }
            } else {
                report
            }
        }
        Err(error) => {
            runtime_failure_report(reference, &fixture, "runtime_capture", error.to_string())
        }
    };
    finish(&args.output_dir, &report);
}

async fn capture(
    reference: ReferenceHardware,
    fixture: &VerticalFlowFixture,
) -> Result<(ProofReport, Vec<u8>), Box<dyn std::error::Error>> {
    let mut runtime = HeadlessRuntime::new(HeadlessConfig {
        width: REFERENCE_WIDTH,
        height: REFERENCE_HEIGHT,
        grpc_port: 0,
        agents: Default::default(),
        config_toml: Some(readback_config_toml()),
    })
    .await?;
    let adapter = runtime.compositor.adapter_info();
    let renderer = RendererIdentity {
        backend: adapter.backend.clone(),
        adapter: adapter.name.clone(),
        device_type: adapter.device_type.clone(),
        driver: adapter.driver.clone(),
        driver_info: adapter.driver_info.clone(),
    };
    {
        let state = runtime.shared_state().lock().await;
        *state.scene.lock().await = fixture.scene.clone();
    }
    let (surface_width, surface_height) = runtime.surface.size();
    runtime.render_frame().await;
    let pixels = runtime.read_pixels();
    let evidence = observe_pixels(
        reference,
        renderer,
        fixture,
        ReadbackDimensions {
            surface_width,
            surface_height,
            render_width: REFERENCE_WIDTH,
            render_height: REFERENCE_HEIGHT,
        },
        &pixels,
    )?;
    Ok((evaluate_evidence(evidence), pixels))
}

fn readback_config_toml() -> String {
    r#"
[runtime]
profile = "headless"

[[tabs]]
name = "Vertical Flow Pixel Proof"
default_tab = true
"#
    .to_string()
}

fn empty_report(reference: ReferenceHardware, fixture: &VerticalFlowFixture) -> ProofReport {
    let evidence = observe_pixels(
        reference,
        RendererIdentity {
            backend: String::new(),
            adapter: String::new(),
            device_type: String::new(),
            driver: String::new(),
            driver_info: String::new(),
        },
        fixture,
        ReadbackDimensions {
            surface_width: REFERENCE_WIDTH,
            surface_height: REFERENCE_HEIGHT,
            render_width: REFERENCE_WIDTH,
            render_height: REFERENCE_HEIGHT,
        },
        &[],
    )
    .unwrap_or_else(|_| ProofEvidence {
        reference_hardware: ReferenceHardware {
            tag: String::new(),
            hostname: String::new(),
            gpu: String::new(),
            gpu_driver: String::new(),
            os: String::new(),
            display_width: 0,
            display_height: 0,
        },
        renderer: RendererIdentity {
            backend: String::new(),
            adapter: String::new(),
            device_type: String::new(),
            driver: String::new(),
            driver_info: String::new(),
        },
        surface_width: REFERENCE_WIDTH,
        surface_height: REFERENCE_HEIGHT,
        render_width: REFERENCE_WIDTH,
        render_height: REFERENCE_HEIGHT,
        pixel_buffer_len: 0,
        children: vec![],
        gap_regions: vec![],
        sentinel_region: None,
    });
    evaluate_evidence(evidence)
}

fn runtime_failure_report(
    reference: ReferenceHardware,
    fixture: &VerticalFlowFixture,
    code: &str,
    detail: String,
) -> ProofReport {
    let mut report = empty_report(reference, fixture);
    report.checks.push(ContractCheck {
        code: code.to_string(),
        passed: false,
        detail,
    });
    report.verdict = ProofVerdict::Fail;
    report
}

fn finish(output_dir: &Path, report: &ProofReport) -> ! {
    let report_path = output_dir.join(REPORT_FILE);
    match serde_json::to_vec_pretty(report)
        .map_err(std::io::Error::other)
        .and_then(|json| fs::write(&report_path, json))
    {
        Ok(()) => println!(
            "{}",
            serde_json::to_string_pretty(report).unwrap_or_default()
        ),
        Err(error) => {
            eprintln!("unable to write {}: {error}", report_path.display());
            std::process::exit(2);
        }
    }
    std::process::exit(if report.verdict == ProofVerdict::Pass {
        0
    } else {
        1
    });
}

fn write_ppm(
    path: &Path,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    if rgba.len() != (width * height * 4) as usize {
        return Err(format!(
            "pixel buffer size mismatch: got {}, expected {}",
            rgba.len(),
            width * height * 4
        )
        .into());
    }
    let file = fs::File::create(path)?;
    let mut writer = BufWriter::new(file);
    write!(writer, "P6\n{width} {height}\n255\n")?;
    for pixel in rgba.chunks_exact(4) {
        writer.write_all(&pixel[..3])?;
    }
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_reference_hardware_contract() {
        let args = parse_args_from(
            [
                "--output",
                "proof",
                "--reference-hardware-tag",
                "TzeHouse",
                "--reference-hostname",
                "reference-windows",
                "--reference-gpu",
                "NVIDIA GeForce RTX 3080",
                "--reference-gpu-driver",
                "32.0.15.9636",
                "--reference-os",
                "Windows 11 Pro 10.0.26200",
                "--display-width",
                "4096",
                "--display-height",
                "2160",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("complete args must parse");
        assert_eq!(args.output_dir, PathBuf::from("proof"));
        assert_eq!(args.reference_hardware_tag, "TzeHouse");
        assert_eq!(args.display_width, 4096);
        assert_eq!(args.display_height, 2160);
    }
}
