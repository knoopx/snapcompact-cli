{
  description = "snapcompact-cli";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/master";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
    in {
      packages = nixpkgs.lib.genAttrs systems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          snapcompact = pkgs.rustPlatform.buildRustPackage {
            pname = "snapcompact-cli";
            version = "0.1.0";
            src = self;
            cargoLock = {
              lockFile = ./Cargo.lock;
            };
            mainProgram = "snapcompact";
            meta = {
              mainProgram = "snapcompact";
              description = "snapcompact CLI";
            };
          };
        in {
          inherit snapcompact;
          default = snapcompact;
        }
      );

      devShells = nixpkgs.lib.genAttrs systems (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
            ];
          };
        }
      );
    };
}
