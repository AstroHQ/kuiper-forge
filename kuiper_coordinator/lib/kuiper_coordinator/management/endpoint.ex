defmodule KuiperCoordinator.Management.Endpoint do
  @moduledoc false
  use GRPC.Endpoint

  run(KuiperCoordinator.Management.Server)
end
