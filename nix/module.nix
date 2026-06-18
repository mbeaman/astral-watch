# NixOS module for astral-watch. On NixOS the systemd unit, udev rule, user, and group are
# declared here rather than shipped as files (the idiomatic Nix way), mirroring
# packaging/astral-watch.service and packaging/99-astral-watch.rules. Enable with:
#   services.astral-watch.enable = true;
{ config, lib, pkgs, ... }:
let
  cfg = config.services.astral-watch;
in
{
  options.services.astral-watch = {
    enable = lib.mkEnableOption "astral-watch ASUS ROG Astral per-pin 12V-2x6 power monitor";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The astral-watch package to run (defaulted by the flake's nixosModule).";
    };

    args = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [
        "log"
        "/var/log/astral-watch/gpu-pins.csv"
        "--interval"
        "0.5"
      ];
      example = [ "monitor" ];
      description = "Arguments to astral-watch (subcommand + flags). Note: the opt-in NVML safety daemon is not built into this package.";
    };
  };

  config = lib.mkIf cfg.enable {
    # /dev/i2c-* access
    boot.kernelModules = [ "i2c-dev" ];

    # the i2c group + an unprivileged system user that belongs to it
    users.groups.i2c = { };
    users.groups.astral-watch = { };
    users.users.astral-watch = {
      isSystemUser = true;
      group = "astral-watch";
      extraGroups = [ "i2c" ];
      description = "astral-watch GPU power monitor";
    };

    # grant the i2c group access to NVIDIA GPU i2c adapters (mirrors packaging/99-astral-watch.rules:
    # scope by the parent PCI device's NVIDIA vendor id, not by adapter name)
    services.udev.extraRules = ''
      SUBSYSTEM=="i2c-dev", ATTRS{vendor}=="0x10de", GROUP="i2c", MODE="0660"
    '';

    systemd.services.astral-watch = {
      description = "astral-watch — ASUS ROG Astral per-pin 12V-2x6 power monitor";
      wantedBy = [ "multi-user.target" ];
      after = [ "multi-user.target" ];
      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} ${lib.escapeShellArgs cfg.args}";
        Restart = "always";
        RestartSec = 5;
        User = "astral-watch";
        Group = "i2c";
        LogsDirectory = "astral-watch";
        # hardening (mirrors packaging/astral-watch.service)
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        ProtectControlGroups = true;
        ProtectKernelTunables = true;
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
        DevicePolicy = "closed";
        DeviceAllow = [ "char-i2c rw" ];
      };
    };
  };
}
