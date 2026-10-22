{ stdenv, libdrm, pkg-config }:

stdenv.mkDerivation {
  pname = "lab-crc";
  version = "0";

  src = ./lab-crc.c;

  dontUnpack = true;
  dontConfigure = true;

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ libdrm ];

  buildPhase = ''
    runHook preBuild
    $CC -Wall -Wextra -Werror -O2 $src $(pkg-config --cflags --libs libdrm) -o lab-crc
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    install -Dm755 lab-crc $out/bin/lab-crc
    runHook postInstall
  '';

  meta = {
    description = "Per-head NVIDIA scanout CRC sampler for the gnomon lab";
    mainProgram = "lab-crc";
  };
}
