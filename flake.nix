{
  description = "GDS testing and tracing";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      python = pkgs.python3.withPackages (ps: [ ps.invoke ]);
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages = [ python pkgs.e2fsprogs pkgs.util-linux ];
      };
    };
}
