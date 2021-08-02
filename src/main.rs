// SPDX-License-Identifier: Apache-2.0/
//
// Some code taken from the coldsnap command line utility
// https://github.com/awslabs/coldsnap/blob/e79156d8ac9f3b82c192a1f774d2ecee89bd7f01/src/bin/coldsnap/main.rs
// Used under the terms of the Apache-2.0 license, Copyright Amazon Inc

use core::str::FromStr;
use std::path::PathBuf;

use anyhow::{ensure, format_err, Context, Result};
use clap::{App, Arg};
use coldsnap::{SnapshotUploader, SnapshotWaiter};
use log::debug;
use rusoto_ebs::EbsClient;
use rusoto_ec2::{Ec2, Ec2Client};
use rusoto_ssm::{GetParametersByPathRequest, Ssm, SsmClient};
use serde::Deserialize;

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

// Converts a string with a number in it to a u64
fn de_string_to_u64<'de, D>(d: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    String::deserialize(d)?
        .parse()
        .map_err(serde::de::Error::custom)
}

#[tokio::main]
async fn main() {
    match main_().await {
        Err(e) => {
            eprintln!("{:?}", e);
            std::process::exit(1);
        }
        Ok(_) => {}
    }
}

async fn main_() -> Result<()> {
    let args = App::new("nixos-ami-upload")
        .about("Upload NixOS AMIs to one or more regions")
        .version(clap::crate_version!())
        .author(clap::crate_authors!())
        .arg(
            Arg::with_name("debug")
                .help("print debug information to stderr")
                .long("debug")
        )
        .arg(
            Arg::with_name("regions")
                .help("list of regions to upload to")
                .takes_value(true)
                .multiple(true)
                .default_value("all")
                .long("regions")
        )
        .arg(
            Arg::with_name("root-size")
                .help("root size of the EBS volume")
                .takes_value(true)
                .long("root-size")
        )
        .arg(
            Arg::with_name("nixos ami directory")
                .help("the path to a nixos 'amazonImage' directory containing 'nix-support/image-info.json'")
                .required(true)
        )
        .get_matches();

    if args.is_present("debug") {
        env_logger::Builder::new()
            .filter(None, log::LevelFilter::Debug)
            .try_init()?;
    }

    let dir = args
        .value_of("nixos ami directory")
        .context("must provide image directory")?;
    let image_info_path = PathBuf::from(dir)
        .join("nix-support")
        .join("image-info.json");

    let f: std::fs::File = std::fs::File::open(&image_info_path).with_context(|| {
        format!(
            "malformed image directory, could not open {:?}",
            &image_info_path
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
    let image = PathBuf::from(info.file);

    gpt::header::read_header(&image, gpt::disk::DEFAULT_SECTOR_SIZE).map_err(|e| {
        format_err!(
            "could not read disk header for disk '{}'. Image must be a valid raw disk image: {}",
            image.to_string_lossy(),
            e
        )
    })?;

    // now for regions
    let region_strs: Vec<_> = args.values_of("regions").unwrap().collect();
    ensure!(
        !region_strs.is_empty(),
        "must specify one or more regions, or use the default of 'all'"
    );
    // If we're given '--regions us-east-1,us-west-2', use the first argument as the first region
    // to uplaod to (the client region).
    // If we're not, upload to the first region based on the default region configured in the aws
    // profile / AWS_REGION env var.
    let mut initial_region = rusoto_core::region::Region::default();
    let resolved_regions = if region_strs[0] == "all" {
        resolve_all_regions().await?
    } else {
        let rs = region_strs
            .into_iter()
            .map(|r| {
                rusoto_core::region::Region::from_str(r).with_context(|| "could not parse region")
            })
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("failed to parse region"))?;
        initial_region = rs[0].clone();
        rs
    };

    debug!("uploading to regions: {:?}", resolved_regions);
    let copy_regions: Vec<_> = resolved_regions
        .into_iter()
        .filter(|f| f != &initial_region)
        .collect();

    // upload time
    debug!("performing initial snapshot upload to {:?}", initial_region);

    let ebs_client = EbsClient::new(initial_region.clone());
    let uploader = SnapshotUploader::new(ebs_client);
    let snapshot_id = uploader
        .upload_from_file(&image, None, Some(&info.label), None)
        .await?;
    println!("uploading snapshot {}", snapshot_id);

    let ec2_client = Ec2Client::new(initial_region.clone());
    SnapshotWaiter::new(ec2_client)
        .wait_for_completed(&snapshot_id)
        .await?;

    // Snapshot done, make the AMI
    let ami_gbs = match args.value_of("root_size") {
        Some(s) => s.parse().context("invalid root-size")?,
        None => {
            // bytes to gbs, but round up
            let bytes_in_gb = 1024 * 1024 * 1024;
            (info.logical_bytes + bytes_in_gb - 1) / bytes_in_gb
        }
    };
    let ec2_client = Ec2Client::new(initial_region.clone());

    let ami_name = format!("NixOS-{}-{}", info.label, info.system);
    let resp = ec2_client
        .register_image(rusoto_ec2::RegisterImageRequest {
            name: ami_name.clone(),
            architecture: Some("x86_64".to_string()),
            description: Some(format!("NixOS {} {}", info.label, info.system)),
            root_device_name: Some("/dev/xvda".to_string()),
            block_device_mappings: Some(vec![
                // Copied from the 'create-amis.sh' script in nixpkgs
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
                rusoto_ec2::BlockDeviceMapping {
                    device_name: Some("/dev/sdb".to_string()),
                    virtual_name: Some("ephemeral0".to_string()),
                    ..Default::default()
                },
                rusoto_ec2::BlockDeviceMapping {
                    device_name: Some("/dev/sdc".to_string()),
                    virtual_name: Some("ephemeral1".to_string()),
                    ..Default::default()
                },
                rusoto_ec2::BlockDeviceMapping {
                    device_name: Some("/dev/sdd".to_string()),
                    virtual_name: Some("ephemeral2".to_string()),
                    ..Default::default()
                },
                rusoto_ec2::BlockDeviceMapping {
                    device_name: Some("/dev/sde".to_string()),
                    virtual_name: Some("ephemeral3".to_string()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        })
        .await
        .expect("could not register ami");
    let init_ami_id = resp.image_id.unwrap();

    println!(
        "registered ami: region={},id={}",
        initial_region.name(),
        init_ami_id
    );

    for region in &copy_regions {
        let ec2_client = Ec2Client::new(region.clone());
        let resp = ec2_client
            .copy_image(rusoto_ec2::CopyImageRequest {
                name: ami_name.clone(),
                source_image_id: init_ami_id.clone(),
                source_region: initial_region.name().to_string(),
                ..Default::default()
            })
            .await
            .expect("could not copy ami to region");
        println!(
            "copied ami to region {} as id {}",
            region.name(),
            resp.image_id.unwrap()
        );
    }
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
        if next_token == None {
            return Ok(result);
        }
    }
}
