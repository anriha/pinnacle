{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:
let
  packages = with pkgs; [
    pkgs.git
    pkgs.wayland
    pkgs.libxkbcommon
    pkgs.fontconfig

    wayland

    # build time stuff
    protobuf
    lua54Packages.luarocks
    lua5_4

    # libs
    seatd.dev
    systemdLibs.dev
    libxkbcommon
    libinput
    mesa
    xwayland
    libdisplay-info
    libgbm
    pkg-config

    # winit on x11
    xorg.libXcursor
    xorg.libXrandr
    xorg.libXi
    xorg.libX11
  ];
in
{
  # https://devenv.sh/basics/
  env.GREET = "devenv";

  inherit packages;

  # https://devenv.sh/packages/

  env.LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath packages;

  # https://devenv.sh/languages/
  languages.rust.enable = true;

  # https://devenv.sh/processes/
  # processes.cargo-watch.exec = "cargo-watch";

  # https://devenv.sh/services/
  # services.postgres.enable = true;

  # https://devenv.sh/scripts/
  scripts.hello.exec = ''
    echo hello from $GREET
  '';

  enterShell = ''
    hello
    git --version
  '';

  # https://devenv.sh/tasks/
  # tasks = {
  #   "myproj:setup".exec = "mytool build";
  #   "devenv:enterShell".after = [ "myproj:setup" ];
  # };

  # https://devenv.sh/tests/
  enterTest = ''
    echo "Running tests"
    git --version | grep --color=auto "${pkgs.git.version}"
  '';

  # https://devenv.sh/git-hooks/
  # git-hooks.hooks.shellcheck.enable = true;

  # See full reference at https://devenv.sh/reference/options/
}
