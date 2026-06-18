{
  description = "Per-pin 12V-2x6 power monitoring and connector-melt early-warning for ASUS ROG Astral GPUs";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "astral-watch";
          inherit version;
          src = self;
          # vendor straight from the committed lockfile — no FOD hash to drift on bumps
          cargoLock.lockFile = ./Cargo.lock;
          # default features only: the lean, read-only build (no tui / no safety / no NVML)
          meta = {
            description = "Per-pin 12V-2x6 power monitoring for ASUS ROG Astral GPUs on Linux";
            homepage = "https://github.com/mbeaman/astral-watch";
            license = pkgs.lib.licenses.mit;
            mainProgram = "astral-watch";
            platforms = pkgs.lib.platforms.linux;
          };
        };
      });

      # `nix run github:mbeaman/astral-watch`
      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.system}.default}/bin/astral-watch";
        };
      });

      # NixOS users: `imports = [ astral-watch.nixosModules.default ];` then
      # `services.astral-watch.enable = true;` — wires the service, udev rule, user, group.
      nixosModules.default =
        { config, lib, pkgs, ... }:
        {
          imports = [ ./nix/module.nix ];
          config = lib.mkIf config.services.astral-watch.enable {
            services.astral-watch.package =
              lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          };
        };
    };
}
