class VeraHarness < Formula
  desc "macOS-first coding agent CLI"
  homepage "https://github.com/virdis-agent/vera-harness"
  version "0.1.0-alpha.9"
  license "MIT"

  on_arm do
    url "https://github.com/virdis-agent/vera-harness/releases/download/v#{version}/vera-#{version}-aarch64-apple-darwin.tar.gz"
    sha256 "6f91e73250fb08fbeffac2414952dae27b839169456b783169aee7692127919d"
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
