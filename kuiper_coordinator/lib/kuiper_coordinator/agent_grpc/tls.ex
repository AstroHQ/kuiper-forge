defmodule KuiperCoordinator.AgentGrpc.TLS do
  @moduledoc """
  TLS for the agent gRPC port: the client cert is optional at the TLS layer (registration runs
  before an agent has one) and enforced per RPC (A1). A client cert that doesn't chain to the coordinator CA
  fails the handshake.

  The server cert doesn't have to come from the coordinator CA, like the Rust coordinator's
  `server_trust_mode = "chain"`: `:server_cert` may be a publicly trusted cert, with its
  intermediates after it in the same PEM file. Agent client certs are still verified only
  against `:ca_cert`, the coordinator CA.

  `check!/1` runs at startup (A4): the files must be readable, the server key must match the
  server cert, and the server cert must verify against the CA agents are told to trust
  (`:server_ca_cert`, else `:ca_cert`; skipped with `server_use_native_roots` in chain mode,
  like Rust), and must be valid for `:server_hostname` when it's set, so a bad path, a
  mismatched pair or a cert for the wrong name stops boot instead of silently failing every
  handshake. An unknown `:server_trust_mode` stops boot too.
  """

  @doc "Validates the files and returns `:ssl` server options."
  def ssl_opts!(opts) do
    %{ca: ca, chain: chain} = check!(opts)

    [
      certs_keys: [
        %{
          cert: Enum.map(chain, &X509.Certificate.to_der/1),
          keyfile: Keyword.fetch!(opts, :server_key)
        }
      ],
      # client certs are verified against the coordinator CA only, whatever signed the server cert
      cacerts: [X509.Certificate.to_der(ca)],
      verify: :verify_peer,
      fail_if_no_peer_cert: false,
      versions: [:"tlsv1.3", :"tlsv1.2"]
    ]
  end

  @doc """
  The CA PEM agents get to verify the server (RegisterResponse.server_ca_pem), as the Rust
  coordinator's `build_server_trust`: `:server_ca_cert`, else `:ca_cert`; none in chain mode
  with native roots.
  """
  def server_ca_pem(opts) do
    case server_ca_file(opts) do
      nil -> ""
      file -> KuiperCoordinator.CA.read!(file, "server CA certificate")
    end
  end

  defp server_ca_file(opts) do
    if Keyword.get(opts, :server_trust_mode, "ca") == "chain" and
         Keyword.get(opts, :server_use_native_roots, false) do
      nil
    else
      opts[:server_ca_cert] || Keyword.fetch!(opts, :ca_cert)
    end
  end

  @trust_modes ~w(ca chain)

  def check!(opts) do
    mode = Keyword.get(opts, :server_trust_mode, "ca")

    unless mode in @trust_modes do
      # agents store the mode they're given and refuse an unknown one after the token is spent
      raise ArgumentError,
            "server trust mode #{inspect(mode)} is not one of #{Enum.join(@trust_modes, ", ")}"
    end

    ca = load_cert!(Keyword.fetch!(opts, :ca_cert), "CA certificate")
    server_file = Keyword.fetch!(opts, :server_cert)
    chain = load_chain!(server_file)
    key_file = Keyword.fetch!(opts, :server_key)

    server_key =
      case X509.PrivateKey.from_pem(KuiperCoordinator.CA.read!(key_file, "server key")) do
        {:ok, key} -> key
        _ -> raise ArgumentError, "server key #{key_file} is not a PEM private key"
      end

    unless KuiperCoordinator.CA.key_matches?(hd(chain), server_key) do
      raise ArgumentError,
            "server key #{key_file} does not match server certificate #{server_file}"
    end

    if trust_file = server_ca_file(opts) do
      trust = load_cert!(trust_file, "server CA certificate")
      # path_validation wants the chain root-most first, ending with the server cert
      path = chain |> Enum.reverse() |> Enum.map(&X509.Certificate.to_der/1)

      case :public_key.pkix_path_validation(X509.Certificate.to_der(trust), path, []) do
        {:ok, _} ->
          :ok

        {:error, {:bad_cert, reason}} ->
          raise ArgumentError,
                "server certificate #{server_file} does not verify against CA #{trust_file}: #{inspect(reason)}"
      end
    end

    if host = opts[:server_hostname] do
      unless :public_key.pkix_verify_hostname(hd(chain), [reference_id(host)]) do
        raise ArgumentError,
              "server certificate #{server_file} is not valid for hostname #{host}"
      end
    end

    %{ca: ca, chain: chain}
  end

  # a numeric host matches an iPAddress SAN, a name a dNSName one
  defp reference_id(host) do
    case :inet.parse_strict_address(String.to_charlist(host)) do
      {:ok, ip} -> {:ip, ip}
      {:error, _} -> {:dns_id, String.to_charlist(host)}
    end
  end

  defp load_cert!(path, what) do
    case X509.Certificate.from_pem(KuiperCoordinator.CA.read!(path, what)) do
      {:ok, cert} -> cert
      _ -> raise ArgumentError, "#{what} #{path} is not a PEM certificate"
    end
  end

  # The server cert, then any intermediates after it in the same file.
  defp load_chain!(path) do
    pem = KuiperCoordinator.CA.read!(path, "server certificate")

    certs =
      for {:Certificate, der, :not_encrypted} <- :public_key.pem_decode(pem),
          do: X509.Certificate.from_der!(der)

    case certs do
      [] -> raise ArgumentError, "server certificate #{path} is not a PEM certificate"
      certs -> certs
    end
  end
end
