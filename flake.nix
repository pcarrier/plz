{
  description = "plz: Jev-backed checks for interactive shell commands";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
    in
    {
      packages = forAllSystems (system: {
        default = nixpkgs.legacyPackages.${system}.rustPlatform.buildRustPackage {
          pname = "plz";
          version = "0.1.0";
          src = nixpkgs.lib.fileset.toSource {
            root = ./.;
            fileset = nixpkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
              ./fish
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          meta.mainProgram = "plz";
        };
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              fish
              sqlite
              nixfmt
            ];
          };
        }
      );

      checks = forAllSystems (system: {
        package = self.packages.${system}.default;
      });
    };
}
