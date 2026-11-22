defmodule KuiperCoordinator.AgentGrpc.Bootstrap do
  @moduledoc """
  First-launch setup of the agent gRPC cert files (D57), so a fresh install needs no manual CA
  step. Runs at startup before the CA is loaded:

    * the data dir (`:data_dir`) and the cert files' folders are made, 0700, when missing;
    * when both `:ca_cert` and `:ca_key` are missing, a CA is made like the Rust coordinator's
      `ca init` (`auth.rs` `init_ca`): ECDSA P-256 / SHA-256, CN "<org> CA", O <org>
      (`:ca_org`, KUIPER_CA_ORG, default "Kuiper Forge"), CA with no path length limit, 3650 days;
    * in trust mode "ca", when both `:server_cert` and `:server_key` are missing, a server cert is
      issued from that CA like `generate_server_cert`: P-256, 365 days, CN = server hostname,
      SAN = the hostname plus localhost and 127.0.0.1, EKU serverAuth.

  Existing files are never overwritten, so a Rust coordinator's CA is used as-is (D2). Half a
  pair, or a CA key that doesn't match its cert, stops startup with a message naming the file.
  Keys are written 0600.
  """
  require Logger
  alias KuiperCoordinator.CA
  alias X509.Certificate.Extension, as: Ext

  @default_org "Kuiper Forge"

  @spec ensure!(keyword) :: :ok
  def ensure!(opts) do
    ca_cert = Keyword.fetch!(opts, :ca_cert)
    ca_key = Keyword.fetch!(opts, :ca_key)
    if dir = opts[:data_dir], do: mkdir!(dir)

    if missing_pair?(ca_cert, ca_key, "CA") do
      init_ca!(ca_cert, ca_key, opts[:ca_org] || @default_org)
    end

    server_cert = opts[:server_cert]
    server_key = opts[:server_key]

    if (Keyword.get(opts, :server_trust_mode, "ca") == "ca" and server_cert) && server_key &&
         missing_pair?(server_cert, server_key, "server") do
      issue_server_cert!(
        CA.load!(ca_cert, ca_key),
        server_cert,
        server_key,
        opts[:server_hostname]
      )
    end

    :ok
  end

  # true when both are missing, false when both exist; half a pair raises
  defp missing_pair?(cert, key, what) do
    case {File.exists?(cert), File.exists?(key)} do
      {false, false} ->
        true

      {true, true} ->
        false

      {cert?, _} ->
        {have, missing} = if cert?, do: {cert, key}, else: {key, cert}

        raise ArgumentError, """
        #{what} file #{missing} is missing but #{have} exists. Restore #{missing}, or remove \
        #{have} too to make a new #{what} pair on the next start (existing files are never \
        overwritten).
        """
    end
  end

  defp init_ca!(cert_file, key_file, org) do
    key = X509.PrivateKey.new_ec(:secp256r1)
    now = DateTime.utc_now()

    cert =
      X509.Certificate.self_signed(key, "/CN=#{org} CA/O=#{org}",
        hash: :sha256,
        validity: X509.Certificate.Validity.new(now, DateTime.add(now, 3650, :day)),
        extensions: [
          basic_constraints: Ext.basic_constraints(true),
          key_usage: Ext.key_usage([:keyCertSign, :cRLSign]),
          subject_key_identifier: true,
          authority_key_identifier: false
        ]
      )

    write_key!(key_file, X509.PrivateKey.to_pem(key))
    write_cert!(cert_file, X509.Certificate.to_pem(cert))
    Logger.info("Created coordinator CA #{cert_file} and key #{key_file} (#{org} CA)")
  end

  defp issue_server_cert!(ca, cert_file, key_file, hostname) do
    host = if hostname in [nil, ""], do: "localhost", else: hostname
    key = X509.PrivateKey.new_ec(:secp256r1)
    now = DateTime.utc_now()

    sans =
      [san(host), {:dNSName, ~c"localhost"}, {:iPAddress, <<127, 0, 0, 1>>}]
      |> Enum.uniq()

    cert =
      key
      |> X509.PublicKey.derive()
      |> X509.Certificate.new("/CN=#{host}/O=CI Runner Coordinator", ca.cert, ca.key,
        hash: :sha256,
        validity: X509.Certificate.Validity.new(now, DateTime.add(now, 365, :day)),
        extensions: [
          basic_constraints: Ext.basic_constraints(false),
          key_usage: Ext.key_usage([:digitalSignature, :keyEncipherment]),
          ext_key_usage: Ext.ext_key_usage([:serverAuth]),
          subject_alt_name: Ext.subject_alt_name(sans),
          subject_key_identifier: true,
          authority_key_identifier: true
        ]
      )

    write_key!(key_file, X509.PrivateKey.to_pem(key))
    write_cert!(cert_file, X509.Certificate.to_pem(cert))
    Logger.info("Created server certificate #{cert_file} and key #{key_file} for #{host}")
  end

  # an IP needs an IP SAN; TLS clients don't match one against DNS names
  defp san(host) do
    case :inet.parse_strict_address(String.to_charlist(host)) do
      {:ok, {a, b, c, d}} -> {:iPAddress, <<a, b, c, d>>}
      {:ok, ip} -> {:iPAddress, for(x <- Tuple.to_list(ip), into: <<>>, do: <<x::16>>)}
      {:error, _} -> {:dNSName, String.to_charlist(host)}
    end
  end

  defp mkdir!(dir) do
    unless File.dir?(dir) do
      dir |> Path.dirname() |> File.mkdir_p!()
      File.mkdir!(dir)
      File.chmod!(dir, 0o700)
      Logger.info("Created data dir #{dir}")
    end
  end

  defp write_key!(path, pem) do
    mkdir!(Path.dirname(path))
    # :exclusive never replaces a file; made 0600 before the key goes in
    File.write!(path, "", [:exclusive])
    File.chmod!(path, 0o600)
    File.write!(path, pem)
  end

  defp write_cert!(path, pem) do
    mkdir!(Path.dirname(path))
    File.write!(path, pem, [:exclusive])
  end
end
