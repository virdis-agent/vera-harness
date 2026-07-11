class VeraHarness < Formula
  desc "macOS-first coding agent CLI"
  homepage "https://github.com/virdis-agent/vera-harness"
  version "0.1.0-alpha.13"
  license "MIT"

  on_arm do
    url "https://github.com/virdis-agent/vera-harness/releases/download/v#{version}/vera-#{version}-aarch64-apple-darwin.tar.gz"
    sha256 "3ee1fb0473346d07f50c6e37177a0b3bdc4c25d942e45c12e7f23babc423ee6a"
  end

  def install
    bin.install "vera"
    zsh_completion.install "completions/vera.zsh" => "_vera"
    bash_completion.install "completions/vera.bash" => "vera"
    fish_completion.install "completions/vera.fish"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/vera --version")
  end
end
