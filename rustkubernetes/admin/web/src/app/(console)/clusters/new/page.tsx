import { Suspense } from "react";
import { NewClusterPage } from "@/features/clusters/NewClusterPage";

export default function Page() {
  return <Suspense><NewClusterPage /></Suspense>;
}
