import { Suspense } from "react";
import { ClusterPage } from "@/features/clusters/ClusterPage";

export default function Page() {
  return <Suspense><ClusterPage /></Suspense>;
}
