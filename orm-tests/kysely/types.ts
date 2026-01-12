import { Generated, ColumnType } from 'kysely';

export interface Database {
  kysely_users: UsersTable;
  kysely_profiles: ProfilesTable;
  kysely_posts: PostsTable;
  kysely_comments: CommentsTable;
  kysely_tags: TagsTable;
  kysely_posts_tags: PostsTagsTable;
  kysely_products: ProductsTable;
  kysely_orders: OrdersTable;
  kysely_order_items: OrderItemsTable;
}

export interface UsersTable {
  id: Generated<number>;
  email: string;
  name: string | null;
  age: number | null;
  is_active: ColumnType<boolean, boolean | undefined, boolean>;
  created_at: ColumnType<Date, Date | undefined, Date>;
}

export interface ProfilesTable {
  id: Generated<number>;
  bio: string | null;
  avatar: string | null;
  user_id: number;
}

export interface PostsTable {
  id: Generated<number>;
  title: string;
  content: string | null;
  published: ColumnType<boolean, boolean | undefined, boolean>;
  author_id: number;
  created_at: ColumnType<Date, Date | undefined, Date>;
}

export interface CommentsTable {
  id: Generated<number>;
  text: string;
  post_id: number;
  created_at: ColumnType<Date, Date | undefined, Date>;
}

export interface TagsTable {
  id: Generated<number>;
  name: string;
}

export interface PostsTagsTable {
  post_id: number;
  tag_id: number;
}

export interface ProductsTable {
  id: Generated<number>;
  name: string;
  price: string;
  stock: ColumnType<number, number | undefined, number>;
}

export interface OrdersTable {
  id: Generated<number>;
  user_id: number;
  total: string;
  status: ColumnType<string, string | undefined, string>;
  created_at: ColumnType<Date, Date | undefined, Date>;
}

export interface OrderItemsTable {
  id: Generated<number>;
  order_id: number;
  product_id: number;
  quantity: number;
  price: string;
}
