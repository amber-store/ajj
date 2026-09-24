{
  description = "ajj: jj with an amber-store backend that syncs with dstore";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    systems.url = "github:nix-systems/default";
  };

  outputs = { self, nixpkgs, systems, ... }:
    let
      lib = nixpkgs.lib;
      eachSystem = f:
        lib.genAttrs (import systems) (system: f system nixpkgs.legacyPackages.${system});
    in
    {
      devShells = eachSystem (system: pkgs: {
        default = pkgs.mkShell {
          hardeningDisable = [ "all" ];
          # go: builds Go dstore for tests/e2e.sh
          packages = with pkgs; [ cargo rustc rustfmt clippy rust-analyzer go ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          GOTOOLCHAIN = "local";
          CGO_ENABLED = "0";
        };
      });
    };
}
