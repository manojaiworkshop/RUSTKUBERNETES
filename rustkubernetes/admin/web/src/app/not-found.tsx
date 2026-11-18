import Link from "next/link";

export default function NotFound() {
  return (
    <div className="grid h-dvh place-items-center p-6 text-center">
      <div>
        <h1 className="mb-2 text-2xl font-semibold">Page not found</h1>
        <Link href="/overview">Back to the overview</Link>
      </div>
    </div>
  );
}
