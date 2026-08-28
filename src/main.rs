// SPDX-License-Identifier: Apache-2.0/
//
// Some code taken from the coldsnap command line utility
// https://github.com/awslabs/coldsnap/blob/e79156d8ac9f3b82c192a1f774d2ecee89bd7f01/src/bin/coldsnap/main.rs
// Used under the terms of the Apache-2.0 license, Copyright Amazon Inc

use core::str::FromStr;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{ensure, format_err, Context, Result};
use coldsnap::{SnapshotUploader, SnapshotWaiter};
use futures::future::try_join_all;
use log::debug;
use rusoto_ebs::EbsClient;
use rusoto_ec2::{Ec2, Ec2Client};
use rusoto_ssm::{GetParametersByPathRequest, Ssm, SsmClient};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use structopt::StructOpt;

const NIX_STORE_PATH_TAG: &str = "NixStorePath";
const NIXOS_NAME_TAG: &str = "NixOSName";
const AMI_WAIT_INTERVAL: Duration = Duration::from_secs(15);
const AMI_WAIT_ATTEMPTS: usize = 480;

#[derive(StructOpt, Debug)]
#[structopt(name = "nixos-ami-upload")]
struct Opt {
    // print debug information to stderr
    #[structopt(long)]
    debug: bool,

    #[structopt(long)]
    name: Option<String>,

    // print progress bars to stderr
    #[structopt(long)]
    progress: Option<bool>,

    // regions to copy the AMI to, or 'all' for all of them.
    #[structopt(long, default_value = "all")]
    regions: String,

    // the root size of the AMI's ebs volume, in GBs. By default, this will be the same as the
    // image's size
    #[structopt(long)]
    root_size: Option<u64>,

    // the output format, one of 'json' or 'nix'
    // AMIs in each region will be printed in this format to stdout on success.
    #[structopt(long, default_value = "json")]
    output_format: OutputFormat,

    // directory containing nixos ami and metadata
    #[structopt(name = "file")]
    ami_dir: String,
}

#[derive(Debug)]
enum OutputFormat {
    Json,
}

impl FromStr for OutputFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "json" => Ok(Self::Json),
            _ => Err(format!("invalid format '{}'; must be 'json'", s)),
        }
    }
}

// Converts a string with a number in it to a u64
fn de_string_to_u64<'de, D>(d: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    String::deserialize(d)?
        .parse()
        .map_err(serde::de::Error::custom)
}

#[derive(Default, Debug, Serialize)]
struct Output {
    // region -> ami ID
    amis: HashMap<String, String>,
}

#[derive(Debug)]
struct CachedAmi {
    image_id: String,
    name: String,
    state: String,
}

// ImageInfo is metadata provided by the nix image build scripts.
// https://github.com/NixOS/nixpkgs/blob/bed52081e58807a23fcb2df38a3f865a2f37834e/nixos/maintainers/scripts/ec2/amazon-image.nix#L86-L92
#[derive(Debug, Deserialize)]
struct ImageInfo {
    label: String,
    system: String,
    #[serde(deserialize_with = "de_string_to_u64")]
    logical_bytes: u64,
    file: PathBuf,
}

#[tokio::main]
async fn main() {
    if let Err(e) = main_().await {
        eprintln!("{:?}", e);
        std::process::exit(1);
    }
}

async fn main_() -> Result<()> {
    let args = Opt::from_args();

    if args.debug {
        env_logger::Builder::new()
            .filter(None, log::LevelFilter::Debug)
            .try_init()?;
    }

    let ami_dir = std::fs::canonicalize(&args.ami_dir)
        .with_context(|| format!("could not resolve AMI directory '{}'", args.ami_dir))?;
    let (store_path, store_hash) = nix_store_identity(&ami_dir)?;
    let image_info_path = ami_dir.join("nix-support").join("image-info.json");

    let f: std::fs::File = std::fs::File::open(&image_info_path).with_context(|| {
        format!(
            "malformed image directory, could not open {:?}",
            image_info_path
        )
    })?;

    let info: ImageInfo = serde_json::from_reader(f).context("error parsing image-info.json")?;

    debug!("read image info: {:?}", info);

    // validation, make sure the image file exists and is probably a raw image
    ensure!(
        info.system == "x86_64-linux",
        "unsupported system '{}'; only x86_64-linux is supported",
        info.system,
    );

    let ami_gbs = match args.root_size {
        Some(s) => s,
        None => {
            let bytes_in_gb = 1024 * 1024 * 1024;
            info.logical_bytes.div_ceil(bytes_in_gb)
        }
    };
    ensure!(ami_gbs > 0, "root volume size must be greater than zero");
    ensure!(ami_gbs <= i64::MAX as u64, "root volume size is too large");

    // now for regions
    let region_strs: Vec<_> = args.regions.split(",").collect();
    ensure!(
        !region_strs.is_empty(),
        "must specify one or more regions, or use the default of 'all'"
    );
    // If we're given '--regions us-east-1,us-west-2', use the first argument as the first region
    // to uplaod to (the client region).
    // If we're not, upload to the first region based on the default region configured in the aws
    // profile / AWS_REGION env var.
    let mut initial_region = rusoto_core::region::Region::default();
    let mut resolved_regions = if region_strs[0] == "all" {
        resolve_all_regions().await?
    } else {
        let rs = region_strs
            .into_iter()
            .map(|r| {
                rusoto_core::region::Region::from_str(r).with_context(|| "could not parse region")
            })
            .collect::<Result<Vec<_>>>()
            .with_context(|| "failed to parse region".to_string())?;
        initial_region = rs[0].clone();
        rs
    };

    // Avoid attempting the same copy more than once when a region is repeated.
    let mut seen_regions = HashSet::new();
    resolved_regions.retain(|region| seen_regions.insert(region.name().to_string()));

    debug!("uploading to regions: {:?}", resolved_regions);
    let nixos_name = format!("NixOS-{}-{}", info.label, info.system);
    let default_ami_name = ami_name_for_store_path(&nixos_name, &store_hash, ami_gbs);
    let requested_ami_name = args.name.unwrap_or(default_ami_name);
    let mut output = Output::default();
    let cache_results = try_join_all(resolved_regions.iter().cloned().map(|region| {
        let store_path = &store_path;
        async move {
            let client = Ec2Client::new(region.clone());
            let ami = find_cached_ami(&client, store_path, ami_gbs).await?;
            Ok::<_, anyhow::Error>((region, ami))
        }
    }))
    .await?;

    let mut cached_amis = Vec::new();
    for (region, cached_ami) in cache_results {
        if let Some(ami) = cached_ami {
            eprintln!(
                "using cached AMI: region={},id={}",
                region.name(),
                ami.image_id
            );
            output
                .amis
                .insert(region.name().to_string(), ami.image_id.clone());
            cached_amis.push((region, ami));
        }
    }

    if output.amis.len() == resolved_regions.len() {
        wait_for_output_amis(&output, &resolved_regions).await?;
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    // A cached AMI in any requested region can be the source for missing copies.
    // Prefer an available AMI so the common cache path does not need to wait.
    cached_amis.sort_by_key(|(_, ami)| ami.state != "available");
    let (source_region, source_ami_id, ami_name) = if let Some((region, ami)) = cached_amis.first()
    {
        if ami.state != "available" {
            eprintln!(
                "waiting for cached AMI {} to become available",
                ami.image_id
            );
            wait_for_ami_available(&Ec2Client::new(region.clone()), &ami.image_id).await?;
        }
        (region.clone(), ami.image_id.clone(), ami.name.clone())
    } else {
        let image = &info.file;
        gpt::header::read_header(image, gpt::disk::DEFAULT_SECTOR_SIZE).map_err(|e| {
            format_err!(
                "could not read disk header for disk '{}'. Image must be a valid raw disk image: {}",
                image.to_string_lossy(),
                e
            )
        })?;

        eprintln!("uploading snapshot to region {}", initial_region.name());
        let progress_bar = match args.progress {
            Some(true) => Some(indicatif::ProgressBar::new(50).with_prefix("snapshot upload")),
            _ => None,
        };
        let uploader = SnapshotUploader::new(EbsClient::new(initial_region.clone()));
        let snapshot_id = uploader
            .upload_from_file(&image, None, Some(&info.label), progress_bar)
            .await?;

        eprintln!("waiting for snapshot upload to finalize");
        SnapshotWaiter::new(Ec2Client::new(initial_region.clone()))
            .wait_for_completed(&snapshot_id)
            .await?;

        eprintln!("registering AMI in {}", initial_region.name());
        let ec2_client = Ec2Client::new(initial_region.clone());
        let resp = ec2_client
            .register_image(register_image_request(
                requested_ami_name.clone(),
                &info,
                snapshot_id,
                ami_gbs,
            ))
            .await
            .context("could not register AMI")?;
        let image_id = resp.image_id.context("RegisterImage returned no AMI ID")?;
        tag_ami(&ec2_client, &image_id, &nixos_name, &store_path).await?;
        output
            .amis
            .insert(initial_region.name().to_string(), image_id.clone());
        eprintln!(
            "registered AMI: region={},id={}",
            initial_region.name(),
            image_id
        );

        (initial_region.clone(), image_id, requested_ami_name)
    };

    let copy_regions: Vec<_> = resolved_regions
        .iter()
        .filter(|region| !output.amis.contains_key(region.name()))
        .cloned()
        .collect();
    if !copy_regions.is_empty() {
        // A newly registered AMI must be available before it can be copied.
        wait_for_ami_available(&Ec2Client::new(source_region.clone()), &source_ami_id).await?;
        let copy_progress =
            indicatif::ProgressBar::new(copy_regions.len() as u64).with_prefix("copying ami");

        for region in &copy_regions {
            let ec2_client = Ec2Client::new(region.clone());
            let resp = ec2_client
                .copy_image(rusoto_ec2::CopyImageRequest {
                    name: ami_name.clone(),
                    source_image_id: source_ami_id.clone(),
                    source_region: source_region.name().to_string(),
                    client_token: Some(copy_client_token(
                        &ami_name,
                        &source_ami_id,
                        source_region.name(),
                        region.name(),
                    )),
                    ..Default::default()
                })
                .await
                .with_context(|| format!("could not copy AMI to {}", region.name()))?;
            let image_id = resp
                .image_id
                .context("CopyImage returned no destination AMI ID")?;
            debug!("created AMI: {}, {}", region.name(), image_id);
            tag_ami(&ec2_client, &image_id, &nixos_name, &store_path).await?;
            output.amis.insert(region.name().to_string(), image_id);
            copy_progress.inc(1);
        }
        copy_progress.finish();
        eprintln!("started AMI copies in all requested regions");
    }

    wait_for_output_amis(&output, &resolved_regions).await?;
    eprintln!("all AMIs are available");

    // And finally, output
    match args.output_format {
        OutputFormat::Json => println!("{}", serde_json::to_string(&output)?),
    };
    Ok(())
}

fn nix_store_identity(path: &Path) -> Result<(String, String)> {
    let relative = path.strip_prefix("/nix/store").with_context(|| {
        format!(
            "AMI directory '{}' is not a canonical Nix store path",
            path.to_string_lossy()
        )
    })?;
    ensure!(
        relative.components().count() == 1,
        "AMI directory must be the root of a Nix store output"
    );
    let basename = relative
        .file_name()
        .and_then(|name| name.to_str())
        .context("Nix store path is not valid UTF-8")?;
    let store_hash = basename
        .split_once('-')
        .map(|(hash, _)| hash)
        .context("Nix store path has no hash prefix")?;
    ensure!(
        store_hash.len() == 32
            && store_hash
                .chars()
                .all(|c| "0123456789abcdfghijklmnpqrsvwxyz".contains(c)),
        "Nix store path has an invalid hash prefix"
    );
    let store_path = path.to_string_lossy().into_owned();
    ensure!(
        store_path.len() <= 256,
        "Nix store path is too long to use as an AWS tag value"
    );
    Ok((store_path, store_hash.to_string()))
}

fn ami_name_for_store_path(nixos_name: &str, store_hash: &str, ami_gbs: u64) -> String {
    let suffix = format!("-{}-{}gb", store_hash, ami_gbs);
    let max_prefix_bytes = 128 - suffix.len();
    let mut prefix_end = nixos_name.len().min(max_prefix_bytes);
    while !nixos_name.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    format!("{}{}", &nixos_name[..prefix_end], suffix)
}

fn copy_client_token(
    ami_name: &str,
    source_ami_id: &str,
    source_region: &str,
    destination_region: &str,
) -> String {
    let mut hasher = Sha256::new();
    for value in [ami_name, source_ami_id, source_region, destination_region] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    format!("nau-{:x}", hasher.finalize())
}

fn register_image_request(
    name: String,
    info: &ImageInfo,
    snapshot_id: String,
    ami_gbs: u64,
) -> rusoto_ec2::RegisterImageRequest {
    rusoto_ec2::RegisterImageRequest {
        name,
        architecture: Some("x86_64".to_string()),
        ena_support: Some(true),
        virtualization_type: Some("hvm".to_string()),
        description: Some(format!("NixOS {} {}", info.label, info.system)),
        root_device_name: Some("/dev/xvda".to_string()),
        block_device_mappings: Some(vec![
            rusoto_ec2::BlockDeviceMapping {
                device_name: Some("/dev/xvda".to_string()),
                ebs: Some(rusoto_ec2::EbsBlockDevice {
                    delete_on_termination: Some(true),
                    volume_type: Some("gp3".to_string()),
                    snapshot_id: Some(snapshot_id),
                    volume_size: Some(ami_gbs as i64),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ephemeral_mapping("/dev/sdb", "ephemeral0"),
            ephemeral_mapping("/dev/sdc", "ephemeral1"),
            ephemeral_mapping("/dev/sdd", "ephemeral2"),
            ephemeral_mapping("/dev/sde", "ephemeral3"),
        ]),
        ..Default::default()
    }
}

fn ephemeral_mapping(device: &str, virtual_name: &str) -> rusoto_ec2::BlockDeviceMapping {
    rusoto_ec2::BlockDeviceMapping {
        device_name: Some(device.to_string()),
        virtual_name: Some(virtual_name.to_string()),
        ..Default::default()
    }
}

async fn tag_ami(
    client: &Ec2Client,
    image_id: &str,
    nixos_name: &str,
    store_path: &str,
) -> Result<()> {
    client
        .create_tags(rusoto_ec2::CreateTagsRequest {
            resources: vec![image_id.to_string()],
            tags: vec![
                rusoto_ec2::Tag {
                    key: Some(NIXOS_NAME_TAG.to_string()),
                    value: Some(nixos_name.to_string()),
                },
                rusoto_ec2::Tag {
                    key: Some(NIX_STORE_PATH_TAG.to_string()),
                    value: Some(store_path.to_string()),
                },
            ],
            ..Default::default()
        })
        .await
        .with_context(|| format!("could not tag AMI {}", image_id))?;
    Ok(())
}

async fn find_cached_ami(
    client: &Ec2Client,
    store_path: &str,
    ami_gbs: u64,
) -> Result<Option<CachedAmi>> {
    let response = client
        .describe_images(rusoto_ec2::DescribeImagesRequest {
            owners: Some(vec!["self".to_string()]),
            filters: Some(vec![
                rusoto_ec2::Filter {
                    name: Some(format!("tag:{}", NIX_STORE_PATH_TAG)),
                    values: Some(vec![store_path.to_string()]),
                },
                rusoto_ec2::Filter {
                    name: Some("block-device-mapping.volume-size".to_string()),
                    values: Some(vec![ami_gbs.to_string()]),
                },
                rusoto_ec2::Filter {
                    name: Some("state".to_string()),
                    values: Some(vec!["available".to_string(), "pending".to_string()]),
                },
            ]),
            ..Default::default()
        })
        .await
        .context("could not search for cached AMIs")?;

    let mut images = response.images.unwrap_or_default();
    images.sort_by(|a, b| {
        let a_available = a.state.as_deref() == Some("available");
        let b_available = b.state.as_deref() == Some("available");
        b_available
            .cmp(&a_available)
            .then_with(|| b.creation_date.cmp(&a.creation_date))
    });
    images
        .into_iter()
        .next()
        .map(|image| {
            Ok(CachedAmi {
                image_id: image.image_id.context("cached AMI has no image ID")?,
                name: image.name.context("cached AMI has no name")?,
                state: image.state.context("cached AMI has no state")?,
            })
        })
        .transpose()
}

async fn wait_for_ami_available(client: &Ec2Client, image_id: &str) -> Result<()> {
    for _ in 0..AMI_WAIT_ATTEMPTS {
        let response = client
            .describe_images(rusoto_ec2::DescribeImagesRequest {
                image_ids: Some(vec![image_id.to_string()]),
                owners: Some(vec!["self".to_string()]),
                ..Default::default()
            })
            .await
            .with_context(|| format!("could not inspect AMI {}", image_id))?;
        let image = response
            .images
            .unwrap_or_default()
            .into_iter()
            .next()
            .with_context(|| format!("AMI {} disappeared while waiting", image_id))?;
        match image.state.as_deref() {
            Some("available") => return Ok(()),
            Some("failed") => {
                return Err(format_err!("AMI {} entered the failed state", image_id));
            }
            _ => tokio::time::sleep(AMI_WAIT_INTERVAL).await,
        }
    }
    Err(format_err!(
        "timed out waiting for AMI {} to become available",
        image_id
    ))
}

async fn wait_for_output_amis(
    output: &Output,
    regions: &[rusoto_core::region::Region],
) -> Result<()> {
    try_join_all(regions.iter().map(|region| async move {
        let image_id = output
            .amis
            .get(region.name())
            .with_context(|| format!("no AMI was created for region {}", region.name()))?;
        wait_for_ami_available(&Ec2Client::new(region.clone()), image_id)
            .await
            .with_context(|| format!("AMI {} in {} is not available", image_id, region.name()))
    }))
    .await?;
    Ok(())
}

async fn resolve_all_regions() -> Result<Vec<rusoto_core::region::Region>> {
    let ssm_client = SsmClient::new(rusoto_core::region::Region::default());
    let mut next_token: Option<String> = None;
    let mut result: Vec<rusoto_core::region::Region> = Vec::new();
    loop {
        let params = ssm_client
            .get_parameters_by_path(GetParametersByPathRequest {
                path: "/aws/service/global-infrastructure/services/ec2/regions".to_string(),
                next_token: next_token.clone(),
                ..Default::default()
            })
            .await?;

        let mut regions = params
            .parameters
            .unwrap()
            .into_iter()
            .map(|p| p.value.unwrap())
            .map(|r| {
                rusoto_core::region::Region::from_str(&r).with_context(|| "could not parse region")
            })
            .collect::<Result<Vec<_>>>()?;
        result.append(&mut regions);

        next_token = params.next_token;
        if next_token.is_none() {
            return Ok(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ami_name_for_store_path, copy_client_token, nix_store_identity};
    use std::path::Path;

    const STORE_HASH: &str = "0123456789abcdfghijklmnpqrsvwxyz";

    #[test]
    fn extracts_nix_store_identity() {
        let path = Path::new("/nix/store/0123456789abcdfghijklmnpqrsvwxyz-example-image");
        let (store_path, hash) = nix_store_identity(path).unwrap();
        assert_eq!(store_path, path.to_str().unwrap());
        assert_eq!(hash, STORE_HASH);
    }

    #[test]
    fn rejects_paths_outside_the_store() {
        assert!(nix_store_identity(Path::new("/tmp/example-image")).is_err());
    }

    #[test]
    fn rejects_paths_below_a_store_output() {
        let path = Path::new("/nix/store/0123456789abcdfghijklmnpqrsvwxyz-example-image/subdir");
        assert!(nix_store_identity(path).is_err());
    }

    #[test]
    fn generated_ami_name_contains_cache_identity_and_fits_aws_limit() {
        let name = ami_name_for_store_path(&"a".repeat(200), STORE_HASH, 16);
        assert_eq!(name.len(), 128);
        assert!(name.ends_with(&format!("{}-16gb", STORE_HASH)));
    }

    #[test]
    fn generated_ami_name_truncates_on_a_character_boundary() {
        let name = ami_name_for_store_path(&"界".repeat(100), STORE_HASH, 16);
        assert!(name.len() <= 128);
        assert!(name.ends_with(&format!("{}-16gb", STORE_HASH)));
    }

    #[test]
    fn generated_ami_names_differ_by_root_volume_size() {
        let small = ami_name_for_store_path("NixOS-test", STORE_HASH, 16);
        let large = ami_name_for_store_path("NixOS-test", STORE_HASH, 100);
        assert_ne!(small, large);
    }

    #[test]
    fn copy_tokens_cover_material_request_context() {
        let token = copy_client_token("ami-name", "ami-source", "us-east-1", "us-west-1");
        assert_eq!(
            token,
            copy_client_token("ami-name", "ami-source", "us-east-1", "us-west-1")
        );
        assert_ne!(
            token,
            copy_client_token("other-name", "ami-source", "us-east-1", "us-west-1")
        );
        assert_ne!(
            token,
            copy_client_token("ami-name", "ami-other", "us-east-1", "us-west-1")
        );
        assert_ne!(
            token,
            copy_client_token("ami-name", "ami-source", "eu-west-1", "us-west-1")
        );
        assert_ne!(
            token,
            copy_client_token("ami-name", "ami-source", "us-east-1", "us-west-2")
        );
        assert!(token.len() <= 128);
    }
}
