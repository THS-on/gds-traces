{
  stdenv,
  symlinkJoin,
  kernel,
}:
let
  module = stdenv.mkDerivation {
    pname = "relay-repro-module";
    version = "0.1";

    src = ../reproducer;

    nativeBuildInputs = kernel.moduleBuildDependencies;

    buildPhase = ''
      runHook preBuild
      make -C "${kernel.dev}/lib/modules/${kernel.modDirVersion}/build" M=$(pwd) modules
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      install -Dm644 relay_repro.ko \
        $out/lib/modules/${kernel.modDirVersion}/extra/relay_repro.ko
      runHook postInstall
    '';

    meta.description = "Kernel module for relay_switch_subbuf smp_mb ordering bug reproducer";
  };

  client = stdenv.mkDerivation {
    pname = "relay-repro-client";
    version = "0.1";

    src = ../reproducer;

    buildPhase = ''
      runHook preBuild
      make client
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      install -Dm755 relay_repro_client $out/bin/relay_repro_client
      runHook postInstall
    '';

    meta.description = "Client for relay_switch_subbuf smp_mb ordering bug reproducer";
  };
in
symlinkJoin {
  name = "relay-repro";
  paths = [
    module
    client
  ];
  meta.description = "Reproducer for relay_switch_subbuf smp_mb ordering bug";
}
