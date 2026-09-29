# Audit 3, B129: the sandbox of the unit, read off the RENDERED unit file.
#
# Evaluation only -- no VM, no system closure is built. What it asserts is
# that the options stand in the unit systemd will read, which is the one
# place a value can silently go missing (a merge, an `mkIf` one level too
# deep, a key in the wrong section). Whether the bot still RUNS under them is
# `nix/test.nix`'s question.
{ pkgs, module, package }:
let
  lib = pkgs.lib;
  system = pkgs.nixos (
    { ... }:
    {
      imports = [ module ];
      boot.isContainer = true;
      system.stateVersion = "26.05";
      services.signal-seerr = {
        enable = true;
        inherit package;
        allowedAddresses = [ "10.0.0.1" "10.0.0.2" ];
        settings = { };
      };
    }
  );
  # Nothing of the system is built for this check: only the text is read,
  # and it does not drag the package or the config file along with it.
  text = builtins.unsafeDiscardStringContext
    system.config.systemd.units."signal-seerr.service".text;
  lines = lib.splitString "\n" text;
  required = [
    "CapabilityBoundingSet="
    "AmbientCapabilities="
    "IPAddressAllow=10.0.0.1"
    "IPAddressAllow=10.0.0.2"
    "IPAddressDeny=any"
    "RestrictAddressFamilies=AF_UNIX"
    "RestrictAddressFamilies=AF_INET"
    "RestrictAddressFamilies=AF_INET6"
    "NoNewPrivileges=true"
    "PrivateDevices=true"
    "ProtectSystem=strict"
    "ProtectHome=true"
    "ProtectKernelModules=true"
    "ProtectKernelLogs=true"
    "ProtectControlGroups=true"
    "ProtectClock=true"
    "ProtectHostname=true"
    "ProtectProc=invisible"
    "ProcSubset=pid"
    "RestrictNamespaces=true"
    "RestrictRealtime=true"
    "RestrictSUIDSGID=true"
    "LockPersonality=true"
    "MemoryDenyWriteExecute=true"
    "SystemCallArchitectures=native"
    "SystemCallFilter=~@privileged"
    "SystemCallErrorNumber=EPERM"
    "UMask=0077"
    "MemoryMax=256M"
  ];
  missing = builtins.filter (l: !(builtins.elem l lines)) required;
  # The other direction: address families beyond the three the bot uses.
  families = builtins.filter (lib.hasPrefix "RestrictAddressFamilies=") lines;
  extraFamilies = builtins.filter (
    l: !(builtins.elem l [
      "RestrictAddressFamilies=AF_UNIX"
      "RestrictAddressFamilies=AF_INET"
      "RestrictAddressFamilies=AF_INET6"
    ])
  ) families;
  problems = missing ++ map (l: "unexpected: ${l}") extraFamilies;
in
pkgs.runCommand "signal-seerr-hardening"
  {
    unit = text;
    problems = lib.concatStringsSep "\n" problems;
  }
  ''
    if [ -n "$problems" ]; then
      echo "the rendered signal-seerr.service lacks:" >&2
      printf '%s\n' "$problems" >&2
      echo "--- rendered unit ---" >&2
      printf '%s\n' "$unit" >&2
      exit 1
    fi
    printf '%s\n' "$unit" > $out
  ''
