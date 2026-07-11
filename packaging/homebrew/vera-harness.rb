class VeraHarness < Formula
  desc "macOS-first coding agent CLI"
  homepage "https://github.com/virdis-agent/vera-harness"
  version "0.1.0-alpha.9"
  license "MIT"

  on_arm do
    url "https://github.com/virdis-agent/vera-harness/releases/download/v#{version}/vera-#{version}-aarch64-apple-darwin.tar.gz"
    sha256 "76a45bd3fd9dbbdb56f70316804ba21cf20e092bd2b2342b040f6a18a53217b0"
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
