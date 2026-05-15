{
  description = "GDS testing and tracing";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    pre-commit-hooks.url = "github:cachix/pre-commit-hooks.nix";
  };

  outputs =
    {
      self,
      nixpkgs,
      pre-commit-hooks,
    }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      python = pkgs.python3.withPackages (ps: [
        ps.invoke
        ps.plotly
      ]);
      hostname = pkgs.lib.strings.trim (builtins.readFile /etc/hostname);
      nixosFlake = builtins.getFlake (
        builtins.unsafeDiscardStringContext (builtins.storePath "/run/booted-system/flake")
      );
      kernel = nixosFlake.outputs.nixosConfigurations.${hostname}.config.boot.kernelPackages.kernel;
    in
    {

      formatter.${system} = pkgs.nixfmt;

      checks.${system}.pre-commit-check = pre-commit-hooks.lib.${system}.run {
        src = ./.;
        hooks = {
          nixfmt.enable = true;
          ruff.enable = true;
          ruff-format.enable = true;
        };
      };

      devShells.${system}.default = pkgs.mkShell {
        packages = [
          python
          pkgs.e2fsprogs
          pkgs.util-linux
          pkgs.fio
          pkgs.perf
        ];
        shellHook = ''
          ${self.checks.${system}.pre-commit-check.shellHook}
        '';
      };
    };
}
