{
  description = "GDS testing and tracing";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    pre-commit-hooks.url = "github:cachix/pre-commit-hooks.nix";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      pre-commit-hooks,
      crane,
    }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      python = pkgs.python3.withPackages (ps: [ ps.invoke ]);
      hostname = pkgs.lib.strings.trim (builtins.readFile /etc/hostname);
      nixosFlake = builtins.getFlake (
        builtins.unsafeDiscardStringContext (builtins.storePath "/run/booted-system/flake")
      );
      kernel = nixosFlake.outputs.nixosConfigurations.${hostname}.config.boot.kernelPackages.kernel;
    in
    {
      packages.${system} = {
        nvme-trace = pkgs.callPackage ./nix/nvme-trace.nix { inherit crane; };
        relay-repro = pkgs.callPackage ./nix/relay-repro.nix { inherit crane kernel pkgs; };
      };

      formatter.${system} = pkgs.nixfmt-rfc-style;

      checks.${system}.pre-commit-check = pre-commit-hooks.lib.${system}.run {
        src = ./.;
        hooks = {
          nixfmt-rfc-style.enable = true;
          ruff.enable = true;
          ruff-format.enable = true;
        };
      };

      devShells.${system}.default = pkgs.mkShell {
        packages = [
          python
          pkgs.e2fsprogs
          pkgs.util-linux
        ];
        shellHook = ''
          ${self.checks.${system}.pre-commit-check.shellHook}
        '';
      };
    };
}
