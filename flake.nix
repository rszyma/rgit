{
  inputs = {
    naersk.url = "github:nix-community/naersk/master";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, utils, naersk }:
    utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        naersk-lib = pkgs.callPackage naersk { };
      in
      {
        defaultPackage = naersk-lib.buildPackage {
          root = ./.;
          nativeBuildInputs = with pkgs; [ pkg-config clang ];
          buildInputs = with pkgs; [ openssl zlib libssh2 libgit2 ];
          LIBCLANG_PATH = "${pkgs.clang.cc.lib}/lib";
          ROCKSDB_LIB_DIR = "${pkgs.rocksdb}/lib";
          LIBSSH2_SYS_USE_PKG_CONFIG = "true";
        };
        devShell = with pkgs; mkShell {
          buildInputs = [ cargo rustc rustfmt pre-commit rustPackages.clippy openssl zlib libssh2 libgit2 pkg-config clang ];
          ROCKSDB_LIB_DIR = "${pkgs.rocksdb}/lib";
          LIBCLANG_PATH = "${pkgs.clang.cc.lib}/lib";
          RUST_SRC_PATH = rustPlatform.rustLibSrc;
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
                default = "/git";
                description = "Path to repositories";
                type = types.path;
              };
              requestTimeout = mkOption {
                default = "10s";
                description = "Timeout for incoming HTTP requests";
                type = types.str;
              };
            };

            config = mkIf cfg.enable {
              users.groups.rgit = { };
              users.users.rgit = {
                description = "RGit service user";
                group = "rgit";
                isSystemUser = true;
                home = "/git";
              };

              systemd.services.rgit = {
                enable = true;
                wantedBy = [ "multi-user.target" ];
                after = [ "network-online.target" ];
                path = [ pkgs.git ];
                serviceConfig = {
                  Type = "exec";
                  ExecStart = "${self.defaultPackage."${system}"}/bin/rgit --request-timeout ${cfg.requestTimeout} --db-store ${cfg.dbStorePath} ${cfg.bindAddress} ${cfg.repositoryStorePath}";
                  Restart = "on-failure";

                  User = "rgit";
                  Group = "rgit";

                  CapabilityBoundingSet = "";
                  NoNewPrivileges = true;
                  PrivateDevices = true;
                  PrivateTmp = true;
                  PrivateUsers = true;
                  PrivateMounts = true;
                  ProtectHome = true;
                  ProtectClock = true;
                  ProtectProc = "noaccess";
                  ProcSubset = "pid";
                  ProtectKernelLogs = true;
                  ProtectKernelModules = true;
                  ProtectKernelTunables = true;
                  ProtectControlGroups = true;
                  ProtectHostname = true;
                  RestrictSUIDSGID = true;
                  RestrictRealtime = true;
                  RestrictNamespaces = true;
                  LockPersonality = true;
                  RemoveIPC = true;
                  RestrictAddressFamilies = [ "AF_INET" "AF_INET6" ];
                  SystemCallFilter = [ "@system-service" "~@privileged" ];
                };
              };
            };
          };
      });
}
