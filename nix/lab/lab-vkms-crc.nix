{ stdenv }:

stdenv.mkDerivation {
  pname = "lab-vkms-crc";
  version = "0";

  src = ./lab-vkms-crc.c;

  dontUnpack = true;
  dontConfigure = true;

  buildPhase = ''
    runHook preBuild
    $CC -Wall -Wextra -Werror -O2 $src -o lab-vkms-crc
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    install -Dm755 lab-vkms-crc $out/bin/lab-vkms-crc
    runHook postInstall
  '';

  meta = {
    description = "vkms scanout CRC sampler for the gnomon lab VM gate";
    mainProgram = "lab-vkms-crc";
  };
}
