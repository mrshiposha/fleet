{ ... }: {
  nixpkgs.overlays = [ (import ../../pkgs) ];
}
