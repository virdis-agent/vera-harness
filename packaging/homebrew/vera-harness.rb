class VeraHarness < Formula
  desc "macOS-first coding agent CLI"
  homepage "https://github.com/virdis-agent/vera-harness"
  version "0.1.0-alpha.8"
  license "MIT"

  on_arm do
    url "https://github.com/virdis-agent/vera-harness/releases/download/v#{version}/vera-#{version}-aarch64-apple-darwin.tar.gz"
    sha256 "5398465fa5c7c7e0e9011cf03d721631590b988972a715606e0e00c1ef5e44fa"
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
