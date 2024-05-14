{...}: {
  nixpkgs.overlays = [
    # Not using craneLib here, because we don't want to have two different rust versions for some platforms.
    (final: prev: {
      fleet-install-secrets = prev.callPackage ({rustPlatform}:
        rustPlatform.buildRustPackage rec {
          pname = "fleet-install-secrets";
          name = "${pname}";

          src = ../.;
          strictDeps = true;

          buildAndTestSubdir = "cmds/install-secrets";

          cargoLock = {
            lockFile = ../Cargo.lock;
            outputHashes = {
              "alejandra-3.0.0" = "sha256-q2oTMen8E1YUbNyU4chPOj728/YR0RzdpN+bNjZX2QU=";
            };
          };
        }) {};
    })
  ];
}
