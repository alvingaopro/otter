{
  description = "workd — dev environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            # Rust (edition 2024, let-chains: needs >= 1.88)
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer

            # Desktop app (apps/desktop): Vite + Tauri CLI via npm
            nodejs_22

            # Runtime deps of workd and its end-to-end tests
            tmux
            git
            direnv
            openssh
          ];

          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };

        formatter = pkgs.nixfmt-rfc-style;
      }
    );
}
