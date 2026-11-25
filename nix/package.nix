{
  lib,
  rustPlatform,
  pkg-config,
  openssl,
  tmux,
  makeWrapper,
  rev ? null,
}:

rustPlatform.buildRustPackage {
  pname = "ai-dev";
  version = "0.1.0";

  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    makeWrapper
  ];
  doCheck = false;
  env = lib.optionalAttrs (rev != null) { AI_DEV_REV = rev; };
  buildInputs = [ openssl ];

  postInstall = ''
    wrapProgram $out/bin/ai-dev --prefix PATH : ${lib.makeBinPath [ tmux ]}
  '';

  meta.mainProgram = "ai-dev";
}
