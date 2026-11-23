# Homebrew formula for saola-mac. The tap repo tonywonguf/homebrew-saola holds a copy
# of this file with the url and sha256 of the release tag.
class Saola < Formula
  desc "Local voice dictation: Handy speech-to-text plus S1-mini cleanup, no cloud"
  homepage "https://github.com/tonywonguf/saola-mac"
  url "https://github.com/tonywonguf/saola-mac/archive/refs/tags/v0.1.1.tar.gz"
  sha256 "@SHA256@"
  license "PolyForm-Noncommercial-1.0.0"
  head "https://github.com/tonywonguf/saola-mac.git", branch: "main"

  depends_on "rust" => :build
  depends_on :macos
  depends_on "ollama"

  def install
    system "cargo", "install", *std_cargo_args(path: "handy-guard")
    libexec.install "libexec/saola-configure-handy"
    pkgshare.install "share/Modelfile.s1-mini", "share/rules.example"
    inreplace "bin/saola" do |s|
      s.gsub! "@LIBEXEC@", libexec.to_s
      s.gsub! "@SHARE@", pkgshare.to_s
    end
    bin.install "bin/saola"
  end

  def caveats
    <<~EOS
      Finish the install with:
        saola setup
      It installs Handy, downloads the S1-mini model (484 MB) and connects the parts.
    EOS
  end

  service do
    run [opt_bin/"handy-guard", "serve", "--listen", "127.0.0.1:11436", "--upstream", "127.0.0.1:11434"]
    keep_alive true
    log_path var/"log/handy-guard.log"
    error_log_path var/"log/handy-guard.log"
  end

  test do
    assert_match "saola #{version}", shell_output("#{bin}/saola version")
    port = free_port
    pid = spawn bin/"handy-guard", "serve", "--listen", "127.0.0.1:#{port}", "--upstream", "127.0.0.1:9"
    begin
      sleep 2
      out = shell_output("curl -s -H 'Host: attacker.example' http://127.0.0.1:#{port}/v1/models")
      assert_match "Host must be", out
    ensure
      Process.kill("TERM", pid)
      Process.wait(pid)
    end
  end
end
