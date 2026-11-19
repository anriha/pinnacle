{
  description = "A WIP Smithay-based Wayland compositor, inspired by AwesomeWM and configured in Lua or Rust";

  inputs = {
    # we require rustc >=1.88
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      fenix,
      ...
    }:
    (flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [
            (final: prev: {
              pinnacle = prev.callPackage ./nix/packages/default.nix { };
            })
          ];
        };
        fenixPkgs = fenix.packages.${system};
        toolchain = fenixPkgs.stable;
        combinedToolchain = toolchain.completeToolchain;
      in
      {
        formatter = pkgs.nixfmt;

        lib = {
          inherit (pkgs.pinnacle) buildRustConfig;
        };

        packages = {
          inherit (pkgs) pinnacle;
          default = pkgs.pinnacle;
        };

        devShell = pkgs.mkShell {
          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.pinnacle.luaEnv
            pkgs.libgbm
          ];
          buildInputs = with pkgs; [
            # rust devel tools
            combinedToolchain
            rust-analyzer
            cargo-outdated
            clang

            # wlcs
            (writeScriptBin "wlcs" ''
              #!/bin/sh
              ${wlcs}/libexec/wlcs/wlcs "$@"
            '')
            # `just wlcs` compiles the pinned wlcs revision from source, so the
            # toolchain it needs has to be in the shell rather than inherited
            # from nixpkgs' wlcs build inputs.
            cmake
            gnumake
            gtest
            boost
            # wlcs' CMakeLists looks these up through pkg-config, and they are
            # separate packages in nixpkgs rather than wayland inputs.
            wayland-scanner
            libffi

            wayland

            # build time stuff
            protobuf
            lua54Packages.luarocks
            lua5_4

            # libs
            seatd
            systemdLibs.dev
            libxkbcommon
            libinput
            mesa
            xwayland
            # The libdisplay-info crate requires <0.4.0; nixpkgs unstable ships 0.4.0.
            libdisplay-info_0_3
            libgbm

            # winit on x11
            libxcursor
            libxrandr
            libxi
            libx11
            alacritty
          ];

          runtimeDependencies = with pkgs; [
            wayland
            mesa
            libglvnd # libEGL
            libgbm
          ];
          NIX_LD_LIBRARY_PATH = "${pkgs.lib.makeLibraryPath (
            with pkgs;
            [
              wayland
              lua5_4
              libinput
              libxkbcommon
              libdisplay-info_0_3
              seatd
              libgbm
              udev
            ]
          )}";
          shellHook = ''
            export LD_LIBRARY_PATH="$LD_LIBRARY_PATH:$NIX_LD_LIBRARY_PATH"
            # `just wlcs` configures wlcs with CMake, which looks for an
            # unprefixed C/C++ compiler that a nix shell does not provide.
            export CC=clang CXX=clang++
          '';
        };
      }
    ))
    // {
      overlays.default = final: prev: {
        pinnacle = prev.callPackage ./nix/packages/default.nix { };
      };

      nixosModules = {
        default = import ./nix/modules/nixos;
      };

      hmModules = {
        default = import ./nix/modules/home-manager;
      };
    };
}
