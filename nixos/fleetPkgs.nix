{ ... }: {
  nixpkgs.overlays = [ (import ../pkgs) ];
}
