{ stdenv, kernel }:
stdenv.mkDerivation {
  pname = "relay-repro";
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

  meta.description = "Reproducer for relay_switch_subbuf smp_mb ordering bug";
}
