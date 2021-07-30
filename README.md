## nixos-ami-upload

In essence, this is [this bash script](https://github.com/NixOS/nixpkgs/blob/bed52081e58807a23fcb2df38a3f865a2f37834e/nixos/maintainers/scripts/ec2/create-amis.sh), but in rust.

It also makes the choice of uploading snapshots directly rather than using the
"vmimport" service, which makes it simpler to operate (no need for an IAM role + s3 bucket to be setup), and in some cases is also faster.

The actual snapshot uploading is done using [coldsnap](https://github.com/awslabs/coldsnap/).

### Usage

1. Create a nixos AMI, as seen [here](https://github.com/euank/nixek-overlay/blob/87cb836fcfc0c7242a9128790737cb0faeeb72c6/amis/jenkins-worker/default.nix#L1-L42).
    Note, the format _must_ be `raw`.
2. Build the nix derivation in 1, such as with `nix-build -o ami my-custom-ami.nix` The output will have a `nix-support/image-info.json` file present if done correctly.
3. Use this tool to upload that ami with `nixos-ami-upload /path/to/nix-build/result --regions us-west-2,us-west-1`.

### Status

At the time of writing, this is neither heavily tested, nor all that clean
code. Caveat emptor.

## License

Apache 2.0, since it borrows code from coldsnap which is similarly licensed.
