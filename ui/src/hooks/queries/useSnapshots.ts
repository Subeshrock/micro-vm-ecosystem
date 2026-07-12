import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { api } from '../../lib/api-client';

export interface Snapshot {
  id: string;
  label?: string;
  created_at: number;
  size_bytes: number;
}

export function useSnapshots(vmId: string | null) {
  return useQuery({
    queryKey: ['snapshots', vmId],
    queryFn: () => api.get<{ snapshots: Snapshot[] }>(`/history/${vmId}`),
    enabled: !!vmId,
  });
}

export function useCreateSnapshot() {
  const queryClient = useQueryClient();
  
  return useMutation({
    mutationFn: ({ vmId, name }: { vmId: string; name?: string }) => 
      api.post(`/snapshot/${vmId}`, { name }),
    onSuccess: (_, { vmId }) => {
      queryClient.invalidateQueries({ queryKey: ['snapshots', vmId] });
    },
  });
}

export function useRestoreSnapshot() {
  return useMutation({
    mutationFn: ({ vmId, snapshotId }: { vmId: string; snapshotId: string }) => 
      api.post(`/restore`, { vm_id: vmId, snapshot_id: snapshotId }),
  });
}
