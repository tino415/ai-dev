{
  lib,
  rustPlatform,
  pkg-config,
  openssl,
  tmux,
  makeWrapper,
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
  buildInputs = [ openssl ];
  nativeCheckInputs = [ tmux ];

  postInstall = ''
    wrapProgram $out/bin/ai-dev --prefix PATH : ${lib.makeBinPath [ tmux ]}
  '';

  meta.mainProgram = "ai-dev";
}
