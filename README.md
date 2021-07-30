## nixos-ami-upload

In essence, this is [this bash script](https://github.com/NixOS/nixpkgs/blob/bed52081e58807a23fcb2df38a3f865a2f37834e/nixos/maintainers/scripts/ec2/create-amis.sh), but in rust.

It also makes the choice of uploading snapshots directly rather than using the
"vmimport" service, which makes it simpler to operate (no need for an AMI role + s3 bucket to bet setup), and in some cases is also faster.

The actual snapshot uploading is done using [coldsnap](https://github.com/awslabs/coldsnap/).

### Status

At the time of writing, this is neither heavily tested, nor all that clean
code. Caveat emptor.

## License

Apache 2.0, since it borrows code from coldsnap which is similarly licensed.
