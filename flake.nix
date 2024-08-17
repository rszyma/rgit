{
  inputs = {
    naersk.url = "github:nix-community/naersk/master";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, utils, naersk, ... }:
    utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        naersk-lib = pkgs.callPackage naersk { };
      in
      {
        defaultPackage = naersk-lib.buildPackage {
          src = pkgs.lib.cleanSource ./.;
          nativeBuildInputs = with pkgs; [ pkg-config clang ];
          buildInputs = with pkgs; [ openssl zlib libssh2 libgit2 ];
          ROCKSDB_LIB_DIR = "${pkgs.rocksdb}/lib";
          LIBCLANG_PATH = "${pkgs.clang.cc.lib}/lib";
          LIBSSH2_SYS_USE_PKG_CONFIG = "true";
        };
        devShell = with pkgs; mkShell {
          packages = [ cargo rustc rustfmt pre-commit rustPackages.clippy pkg-config clang ];
          buildInputs = [ openssl zlib libssh2 libgit2 ];
          RUST_SRC_PATH = rustPlatform.rustLibSrc;
          ROCKSDB_LIB_DIR = "${rocksdb}/lib";
          LIBCLANG_PATH = "${clang.cc.lib}/lib";
          LIBSSH2_SYS_USE_PKG_CONFIG = "true";
        };

        nixosModules.default = { config, lib, pkgs, ... }:
          with lib;
          let
            cfg = config.services.rgit;
          in
          {
            options.services.rgit = {
              enable = mkEnableOption "rgit";
              bindAddress = mkOption {
                default = "[::]:8333";
                description = "Address and port to listen on";
                type = types.str;
              };
              dbStorePath = mkOption {
                default = "/tmp/rgit.db";
                description = "Path to store the temporary cache";
                type = types.path;
              };
              repositoryStorePath = mkOption {
                example = "/path/to/git/repos";
                description = "Path to repositories";
                type = types.path;
              };
              scanExclude = mkOption {
                default = null;
                example = "gitolite-admin|mysecrets";
                description = "Exclude the Git repositories that this regex finds.";
                type = types.nullOr types.str;
              };
              requestTimeout = mkOption {
                default = "10s";
                description = "Timeout for incoming HTTP requests";
                type = types.str;
              };
              refreshInterval = mkOption {
                default = "1m";
                description = "Git repositories index refresh interval";
                type = types.str;
              };
            };

            config = mkIf cfg.enable {
              # users.groups.rgit = { };
              # users.users.git.extraGroups = [ "rgit" ];
              # users.users.rgit = {
              #   description = "RGit service user";
              #   group = "rgit";
              #   isSystemUser = true;
              # };

              systemd.services.rgit = {
                enable = true;
                wantedBy = [ "multi-user.target" ];
                requires = [ "network-online.target" ];
                after = [ "network-online.target" ];
                path = [ pkgs.git ];
                serviceConfig = {
                  Type = "exec";
                  ExecStart = builtins.concatStringsSep " " [
                    "${self.defaultPackage."${system}"}/bin/rgit"
                    "--db-store '${cfg.dbStorePath}'"
                    (if cfg.scanExclude != null then "--scan-exclude '${cfg.scanExclude}'" else "")
                    "--request-timeout '${cfg.requestTimeout}'"
                    "--refresh-interval '${cfg.refreshInterval}'"
                    "'${cfg.bindAddress}'"
                    "'${cfg.repositoryStorePath}'"
                  ];
                  Restart = "on-failure";

                  User = "git";
                  Group = "git";

                  # CapabilityBoundingSet = "";
                  # NoNewPrivileges = true;
                  # PrivateDevices = true;
                  # PrivateTmp = true;
                  # PrivateUsers = true;
                  # PrivateMounts = true;
                  # ProtectHome = true;
                  # ProtectClock = true;
                  # ProtectProc = "noaccess";
                  # ProcSubset = "pid";
                  # ProtectKernelLogs = true;
                  # ProtectKernelModules = true;
                  # ProtectKernelTunables = true;
                  # ProtectControlGroups = true;
                  # ProtectHostname = true;
                  # RestrictSUIDSGID = true;
                  # RestrictRealtime = true;
                  # RestrictNamespaces = true;
                  # LockPersonality = true;
                  # RemoveIPC = true;
                  # RestrictAddressFamilies = [ "AF_INET" "AF_INET6" ];
                  # SystemCallFilter = [ "@system-service" "~@privileged" ];
                };
              };
            };
          };
      });
}
