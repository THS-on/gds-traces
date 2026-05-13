{
  crane,
  pkgs,
}:
let
  craneLib = crane.mkLib pkgs;
  src = craneLib.cleanCargoSource ../tracing/nvme-trace;
  commonArgs = {
    inherit src;
    strictDeps = true;
  };
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
craneLib.buildPackage (
  commonArgs
  // {
    inherit cargoArtifacts;
  }
)
