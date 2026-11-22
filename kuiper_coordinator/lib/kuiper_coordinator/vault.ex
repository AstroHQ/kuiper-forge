defmodule KuiperCoordinator.Vault do
  @moduledoc """
  Encrypts stored secrets (GitHub App private keys, Forgejo tokens, webhook secrets) at rest
  with AES-256-GCM (D39). The key is `config :kuiper_coordinator, KuiperCoordinator.Vault,
  key: <32 bytes>`, which `config/runtime.exs` reads from `KUIPER_ENCRYPTION_KEY` (base64).
  Lose the key and the stored secrets can't be read: re-enter them in the admin UI.
  """
  use Cloak.Vault, otp_app: :kuiper_coordinator

  @impl GenServer
  def init(config) do
    key = Keyword.get(config, :key)

    ciphers =
      if is_binary(key) and byte_size(key) == 32,
        do: [default: {Cloak.Ciphers.AES.GCM, tag: "AES.GCM.V1", key: key, iv_length: 12}],
        else: []

    {:ok, Keyword.put(config, :ciphers, ciphers)}
  end
end

defmodule KuiperCoordinator.Encrypted.Binary do
  @moduledoc "An Ecto type for a secret stored encrypted by `KuiperCoordinator.Vault`."
  use Cloak.Ecto.Binary, vault: KuiperCoordinator.Vault
end
