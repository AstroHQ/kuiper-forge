defmodule KuiperCoordinator.Forge.QueuedJob do
  @moduledoc "A job waiting for a runner, found by a forge's recovery scan (Rust forge.rs QueuedJob)."
  alias KuiperCoordinator.Forge.RunnerScope

  @enforce_keys [:id, :labels, :runner_scope]
  defstruct [:id, :labels, :runner_scope, :repository, :created_at]

  @type t :: %__MODULE__{
          id: String.t(),
          labels: [String.t()],
          runner_scope: RunnerScope.t(),
          repository: String.t() | nil,
          created_at: DateTime.t() | nil
        }
end
