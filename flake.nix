{
  description = ''
    To develop run 'nix develop', 
    then 'cargo run --' to run the Rust app

    To build run 'nix build .#default', 
    then 'nix shell .#default' 
    and then whatever the name of the program is
  '';

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    naersk.url = "github:nix-community/naersk";
  };

  outputs = { self, nixpkgs, naersk }: 
    let
      supportedSystems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in 
    {
      devShells = forAllSystems (system:
        let 
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            name = "nix-rust";
            buildInputs = with pkgs; [ cargo rustc rustfmt clippy rust-analyzer glib ];
            nativeBuildInputs = with pkgs; [ pkg-config ];
            env.RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
            shellHook = ''
              eval "$(starship init bash)"
            '';
          };
        }
      );

      packages = forAllSystems (system:
        let 
          pkgs = nixpkgs.legacyPackages.${system};
	  naerskLib = pkgs.callPackage naersk {};
        in
        {
          default = naerskLib.buildPackage {
            name = "name";
	    version = "1.0.0";
            src = ./.;

            #cargoLock = { lockFile = ./Cargo.lock; };

            buildInputs = with pkgs; [ 
	      glib
	    ];
            nativeBuildInputs = with pkgs; [ 
	      pkg-config
	    ];
	    meta = with pkgs.lib; {
	      description = "Package description";
	      homepage = "https://...";
	      license = licenses.gpl3;
	      platforms = platforms.linux;
	    };
          };
        }
      );
    };
}
