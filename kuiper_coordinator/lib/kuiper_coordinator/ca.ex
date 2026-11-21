defmodule KuiperCoordinator.CA do
  @moduledoc """
  The coordinator CA, loaded from the same PEM files the Rust coordinator uses (decision D2),
  so client certs it already issued stay valid. Issues agent client certs like `auth.rs`.

  Agent certs are signed with the loaded CA cert's real subject as issuer (D7); for a CA made by
  the Rust `init_ca` that is the same DN the Rust coordinator writes.
  """
  alias X509.Certificate.Extension, as: Ext

  @key {__MODULE__, :loaded}
  @max_agent_id 64
  @alnum ~c"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"

  defstruct [:cert, :key, :pem, :cert_file]

  @doc "Reads and checks the CA cert and key, raising with a clear message when they're unusable."
  @spec load!(Path.t(), Path.t()) :: %__MODULE__{}
  def load!(cert_file, key_file) do
    pem = read!(cert_file, "CA certificate")
    key_pem = read!(key_file, "CA key")

    cert =
      case X509.Certificate.from_pem(pem) do
        {:ok, cert} -> cert
        _ -> raise ArgumentError, "CA certificate #{cert_file} is not a PEM certificate"
      end

    key =
      case X509.PrivateKey.from_pem(key_pem) do
        {:ok, key} -> key
        _ -> raise ArgumentError, "CA key #{key_file} is not a PEM private key"
      end

    unless key_matches?(cert, key) do
      raise ArgumentError, "CA key #{key_file} does not match CA certificate #{cert_file}"
    end

    %__MODULE__{cert: cert, key: key, pem: pem, cert_file: cert_file}
  end

  @doc "Loads the CA and makes it the one `issue_agent_cert/2` and `pem/0` use."
  def install!(cert_file, key_file) do
    ca = load!(cert_file, key_file)
    :persistent_term.put(@key, ca)
    ca
  end

  def loaded, do: :persistent_term.get(@key, nil)

  def pem do
    case loaded() do
      %__MODULE__{pem: pem} -> pem
      nil -> raise "CA not loaded"
    end
  end

  @doc false
  def read!(path, what) do
    case File.read(path) do
      {:ok, data} ->
        data

      {:error, reason} ->
        raise ArgumentError, "cannot read #{what} #{path}: #{:file.format_error(reason)}"
    end
  end

  @doc "True when the private key belongs to the certificate."
  def key_matches?(cert, key) do
    X509.Certificate.public_key(cert) == X509.PublicKey.derive(key)
  end

  @doc """
  agent_id = `agent_{type}_{sanitized hostname}_{8 alnum}` (auth.rs:483-488). Sanitizing keeps
  alphanumerics and `-`, maps everything else to `-`, then lowercases.
  """
  def new_agent_id(agent_type, hostname) do
    # the id is the cert CN, which X.509 caps at 64 characters: shorten the hostname part
    room = @max_agent_id - String.length("agent_#{agent_type}__") - 8
    host = hostname |> sanitize_hostname() |> String.slice(0, max(room, 0))
    "agent_#{agent_type}_#{host}_#{random_string(8)}"
  end

  def sanitize_hostname(hostname) do
    hostname
    |> String.replace(~r/[^[:alnum:]-]/u, "-")
    |> String.downcase()
  end

  @doc """
  `n` random alphanumeric characters from `:crypto.strong_rand_bytes/1` (registration tokens
  are bearer secrets). Bytes >= 248 are dropped so each of the 62 characters is equally likely.
  """
  def random_string(n), do: random_string(n, "")

  defp random_string(0, acc), do: acc

  defp random_string(n, acc) do
    chars =
      for <<b <- :crypto.strong_rand_bytes(n * 2)>>, b < 248, into: "" do
        <<Enum.at(@alnum, rem(b, 62))>>
      end

    take = min(n, byte_size(chars))
    random_string(n - take, acc <> binary_part(chars, 0, take))
  end

  @doc """
  Issues an agent client cert (auth.rs:491-524): server-made P-256 key, 365 days,
  CN = SAN DNS = agent_id, O = "CI Runner Agent", KU digitalSignature, EKU clientAuth.
  """
  def issue_agent_cert(agent_id, ca \\ loaded()) do
    key = X509.PrivateKey.new_ec(:secp256r1)
    now = DateTime.utc_now()
    expires_at = DateTime.add(now, 365, :day)

    cert =
      key
      |> X509.PublicKey.derive()
      |> X509.Certificate.new("/O=CI Runner Agent/CN=#{agent_id}", ca.cert, ca.key,
        hash: :sha256,
        validity: X509.Certificate.Validity.new(DateTime.add(now, -60, :second), expires_at),
        extensions: [
          basic_constraints: Ext.basic_constraints(false),
          key_usage: Ext.key_usage([:digitalSignature]),
          ext_key_usage: Ext.ext_key_usage([:clientAuth]),
          subject_alt_name: Ext.subject_alt_name([agent_id]),
          subject_key_identifier: false,
          authority_key_identifier: true
        ]
      )

    %{
      cert_pem: X509.Certificate.to_pem(cert),
      key_pem: X509.PrivateKey.to_pem(key, wrap: true),
      serial: Base.encode16(X509.Certificate.to_der(cert), case: :lower),
      expires_at: expires_at
    }
  end

  @doc "Agent identity: the first CN of a DER peer cert (server.rs extract_agent_id_from_cert)."
  def agent_id_from_der(der) when is_binary(der) do
    der |> X509.Certificate.from_der!() |> X509.Certificate.subject("CN") |> List.first()
  rescue
    _ -> nil
  end
end
