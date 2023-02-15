{ pkgs ? import <nixpkgs> {} }:
  pkgs.mkShell {
  buildInputs = with pkgs; [
    pkg-config
    alsa-lib
    cmake
    openssl
    clang
    libclang
    xorg.xcbutil
    xorg.libxcb.dev
    xorg.libxcb
    xorg.xcbutilrenderutil
  ];
  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (with pkgs; [
    xorg.libX11
    xorg.libXcursor
    xorg.libXrandr
    xorg.libXi
    openssl
    vulkan-loader
    clang
    stdenv.cc.cc.lib
    libclang
    cmake
  ]);
}
