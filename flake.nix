{
  description = "A very basic flake";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
  };

  outputs = { self, nixpkgs }: let
    pkgs = nixpkgs.legacyPackages."aarch64-linux";
  in {
    devShells."aarch64-linux".default = pkgs.mkShell {
      buildInputs = with pkgs; [
        cargo
	rustc
	rustfmt
	clippy
	rust-analyzer
      ];
      env.RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
    };
  };
}
