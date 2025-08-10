{ pkgs ? import <nixpkgs> {} }: pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    # building
    cargo
    pkg-config
    openssl
    sqlite
  ];
}
