defmodule KuiperCoordinator.Repo do
  use Ecto.Repo,
    otp_app: :kuiper_coordinator,
    adapter: Ecto.Adapters.Postgres
end
